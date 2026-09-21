//! Binding a machine or a phone to an account: the device credential.
//!
//! `POST /v1/devices` mints one. It is authenticated either by the
//! one-time bootstrap token (while the RC instance has no account yet) or
//! by an existing device's access token (adding another device to the same
//! account). The answer carries a long-lived refresh token, which is what a
//! device keeps, and a short-lived access token, which it exchanges the
//! refresh token for again at `POST /v1/devices/token` whenever it has to.
//!
//! | Route | Body | Credential |
//! |---|---|---|
//! | `POST /v1/devices` | [`IssueDeviceRequest`] → [`IssuedDevice`] (201) | bootstrap token **or** device access token |
//!
//! Pure data, defined in this crate alone: the server serializes these
//! types and the client deserializes them, so one definition keeps the two
//! from drifting apart.

use serde::{Deserialize, Serialize};

/// Longest device label RC stores, in characters.
pub const MAX_DEVICE_LABEL_CHARS: usize = 200;

/// Body of `POST /v1/devices`. Every field is optional, and so is the body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueDeviceRequest {
    /// Human-readable name for the device, shown when devices are listed.
    /// 1 to [`MAX_DEVICE_LABEL_CHARS`] characters; RC uses `device` when it
    /// is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Answer of `POST /v1/devices`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedDevice {
    /// The account the device belongs to.
    pub account_id: String,
    /// The new device.
    pub device_id: String,
    /// Long-lived; exchanged for access tokens. The one thing to keep.
    pub refresh_token: String,
    /// Short-lived bearer for device-scoped routes.
    pub access_token: String,
    /// When `access_token` stops working (RFC 3339).
    pub access_expires_at: String,
}

/// Redacted: both tokens are bearer credentials.
impl std::fmt::Debug for IssuedDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedDevice")
            .field("account_id", &self.account_id)
            .field("device_id", &self.device_id)
            .field("refresh_token", &"<redacted>")
            .field("access_token", &"<redacted>")
            .field("access_expires_at", &self.access_expires_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_issued_device_keeps_its_wire_keys() {
        let issued = IssuedDevice {
            account_id: "acct_1".into(),
            device_id: "dev_1".into(),
            refresh_token: "refresh".into(),
            access_token: "access".into(),
            access_expires_at: "2026-09-16T00:00:00Z".into(),
        };
        let value = serde_json::to_value(&issued).unwrap();
        assert_eq!(
            value,
            json!({
                "account_id": "acct_1",
                "device_id": "dev_1",
                "refresh_token": "refresh",
                "access_token": "access",
                "access_expires_at": "2026-09-16T00:00:00Z"
            })
        );
        assert_eq!(
            serde_json::from_value::<IssuedDevice>(value).unwrap(),
            issued
        );
    }

    #[test]
    fn debug_never_prints_a_token() {
        let rendered = format!(
            "{:?}",
            IssuedDevice {
                account_id: "acct_1".into(),
                device_id: "dev_1".into(),
                refresh_token: "hunter2".into(),
                access_token: "swordfish".into(),
                access_expires_at: String::new(),
            }
        );
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("swordfish"), "{rendered}");
        assert!(rendered.contains("dev_1"), "{rendered}");
    }

    #[test]
    fn the_request_body_is_optional_and_strict() {
        assert_eq!(
            serde_json::to_value(IssueDeviceRequest::default()).unwrap(),
            json!({})
        );
        assert_eq!(
            serde_json::from_value::<IssueDeviceRequest>(json!({"label": "laptop"})).unwrap(),
            IssueDeviceRequest {
                label: Some("laptop".into())
            }
        );
        assert!(serde_json::from_value::<IssueDeviceRequest>(json!({"lable": "x"})).is_err());
    }
}
