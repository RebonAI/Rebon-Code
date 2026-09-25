//! The device authorization grant (RFC 8628), as decisions.
//!
//! The user reads a short code off the terminal, types it on a page the
//! provider hosts, and approves there; meanwhile this side asks the token
//! endpoint, at an interval the server sets, whether they have. What each
//! answer means and how long to wait before the next question are pure
//! rules, so they live here: the HTTP and the JSON are
//! [`crate::oauth::device`]'s, and it hands this module fields it already
//! read.
//!
//! ## Pinned rules
//!
//! 1. The answer is read from the `error` field, not the status: GitHub
//!    answers a pending grant with `200` and an `error`, other servers
//!    with `400`. An `error` always wins over a token in the same body.
//! 2. `authorization_pending` keeps the interval; `slow_down` adds
//!    [`SLOW_DOWN_STEP`] (RFC 8628 §3.5), or takes the server's `interval`
//!    when that is longer.
//! 3. `expired_token` and `access_denied` end the flow; so does any error
//!    this module does not know, and a success with no `access_token`.
//! 4. A missing or zero `interval` is [`DEFAULT_INTERVAL`]; the flow ends
//!    by itself once the waits add up to the code's `expires_in`.

use std::time::Duration;

/// RFC 8628 §3.2: "If no value is provided, clients MUST use 5 as the
/// default."
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// RFC 8628 §3.5: on `slow_down` "the interval MUST be increased by 5
/// seconds for this and all subsequent requests".
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// What the device authorization endpoint handed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    /// Sent back on every poll; never shown.
    pub device_code: String,
    /// What the user types on the verification page.
    pub user_code: String,
    pub verification_uri: String,
    /// The same page with the code already filled in, when offered.
    pub verification_uri_complete: Option<String>,
    /// How long the code is good for.
    pub expires_in: Duration,
    /// How long to wait between polls.
    pub interval: Duration,
}

impl DeviceAuthorization {
    /// The page to open for the user: the pre-filled one when there is one.
    pub fn page_to_open(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// The fields of one token-endpoint answer that decide what happens next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollFields {
    pub error: Option<String>,
    pub error_description: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// Seconds.
    pub expires_in: Option<u64>,
    /// Seconds; a `slow_down` may carry the interval the server wants.
    pub interval: Option<u64>,
}

/// What one poll answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollAnswer {
    /// The user has not finished yet; ask again after the interval.
    Pending,
    /// Asked too often; ask again after a longer interval.
    SlowDown { server_interval: Option<Duration> },
    /// The code ran out before the user approved.
    Expired,
    /// The user declined.
    Denied,
    /// Approved.
    Granted {
        access_token: String,
        refresh_token: Option<String>,
        /// Seconds, when the token expires at all.
        expires_in: Option<u64>,
    },
    /// Anything else. The text is the server's, for the user.
    Failed(String),
}

/// Read one poll. `status` only matters when there is no `error` field.
pub fn classify_poll(status: u16, fields: PollFields) -> PollAnswer {
    if let Some(error) = fields.error.as_deref().filter(|e| !e.is_empty()) {
        return match error {
            "authorization_pending" => PollAnswer::Pending,
            "slow_down" => PollAnswer::SlowDown {
                server_interval: fields.interval.map(Duration::from_secs),
            },
            "expired_token" => PollAnswer::Expired,
            "access_denied" => PollAnswer::Denied,
            other => PollAnswer::Failed(match fields.error_description {
                Some(description) if !description.is_empty() => {
                    format!("{other}: {description}")
                }
                _ => other.to_string(),
            }),
        };
    }
    if !(200..300).contains(&status) {
        return PollAnswer::Failed(format!("the token endpoint answered {status}"));
    }
    match fields.access_token.filter(|token| !token.is_empty()) {
        Some(access_token) => PollAnswer::Granted {
            access_token,
            refresh_token: fields.refresh_token.filter(|token| !token.is_empty()),
            expires_in: fields.expires_in,
        },
        None => PollAnswer::Failed("the token endpoint answered without a token".to_string()),
    }
}

/// When to ask next, and when to stop asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollSchedule {
    interval: Duration,
    remaining: Duration,
}

impl PollSchedule {
    /// Start from the authorization's interval and lifetime.
    pub fn new(authorization: &DeviceAuthorization) -> Self {
        Self {
            interval: normalize_interval(authorization.interval),
            remaining: authorization.expires_in,
        }
    }

    /// How long to wait before the next poll.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Record one wait of [`Self::interval`]. Returns `false` once the code
    /// has run out, so the caller stops without asking a question whose
    /// answer can only be `expired_token`.
    pub fn waited(&mut self) -> bool {
        self.remaining = self.remaining.saturating_sub(self.interval);
        !self.remaining.is_zero()
    }

    /// Apply a `slow_down`.
    pub fn slow_down(&mut self, server_interval: Option<Duration>) {
        let stepped = self.interval + SLOW_DOWN_STEP;
        self.interval = server_interval
            .map(|asked| asked.max(stepped))
            .unwrap_or(stepped);
    }
}

fn normalize_interval(interval: Duration) -> Duration {
    if interval.is_zero() {
        DEFAULT_INTERVAL
    } else {
        interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(error: Option<&str>, token: Option<&str>) -> PollFields {
        PollFields {
            error: error.map(str::to_string),
            access_token: token.map(str::to_string),
            ..PollFields::default()
        }
    }

    fn authorization(interval: u64, expires_in: u64) -> DeviceAuthorization {
        DeviceAuthorization {
            device_code: "d".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            verification_uri_complete: None,
            expires_in: Duration::from_secs(expires_in),
            interval: Duration::from_secs(interval),
        }
    }

    #[test]
    fn each_documented_error_maps_to_its_answer_whatever_the_status() {
        for status in [200, 400] {
            assert_eq!(
                classify_poll(status, fields(Some("authorization_pending"), None)),
                PollAnswer::Pending
            );
            assert_eq!(
                classify_poll(status, fields(Some("expired_token"), None)),
                PollAnswer::Expired
            );
            assert_eq!(
                classify_poll(status, fields(Some("access_denied"), None)),
                PollAnswer::Denied
            );
            assert_eq!(
                classify_poll(status, fields(Some("slow_down"), None)),
                PollAnswer::SlowDown {
                    server_interval: None
                }
            );
        }
    }

    #[test]
    fn slow_down_carries_the_servers_interval() {
        let mut f = fields(Some("slow_down"), None);
        f.interval = Some(12);
        assert_eq!(
            classify_poll(200, f),
            PollAnswer::SlowDown {
                server_interval: Some(Duration::from_secs(12))
            }
        );
    }

    /// An error in the same body as a token wins: fail closed.
    #[test]
    fn an_error_beats_a_token_in_the_same_answer() {
        assert_eq!(
            classify_poll(200, fields(Some("authorization_pending"), Some("t"))),
            PollAnswer::Pending
        );
    }

    #[test]
    fn a_grant_carries_its_tokens_and_drops_empty_ones() {
        let answer = classify_poll(
            200,
            PollFields {
                access_token: Some("gho".into()),
                refresh_token: Some(String::new()),
                expires_in: Some(28800),
                ..PollFields::default()
            },
        );
        assert_eq!(
            answer,
            PollAnswer::Granted {
                access_token: "gho".into(),
                refresh_token: None,
                expires_in: Some(28800),
            }
        );
    }

    #[test]
    fn unknown_errors_empty_grants_and_bare_failures_end_the_flow() {
        let mut unknown = fields(Some("incorrect_client_credentials"), None);
        unknown.error_description = Some("bad client".into());
        assert_eq!(
            classify_poll(200, unknown),
            PollAnswer::Failed("incorrect_client_credentials: bad client".into())
        );
        assert!(matches!(
            classify_poll(200, fields(None, None)),
            PollAnswer::Failed(_)
        ));
        assert!(matches!(
            classify_poll(200, fields(None, Some(""))),
            PollAnswer::Failed(_)
        ));
        assert!(matches!(
            classify_poll(500, fields(None, Some("t"))),
            PollAnswer::Failed(_)
        ));
        // An empty `error` is not an error.
        assert!(matches!(
            classify_poll(200, fields(Some(""), Some("t"))),
            PollAnswer::Granted { .. }
        ));
    }

    #[test]
    fn slow_down_steps_by_five_seconds_or_takes_a_longer_server_interval() {
        let mut schedule = PollSchedule::new(&authorization(5, 900));
        assert_eq!(schedule.interval(), Duration::from_secs(5));
        schedule.slow_down(None);
        assert_eq!(schedule.interval(), Duration::from_secs(10));
        schedule.slow_down(Some(Duration::from_secs(30)));
        assert_eq!(schedule.interval(), Duration::from_secs(30));
        // A shorter server interval does not undo the step.
        schedule.slow_down(Some(Duration::from_secs(1)));
        assert_eq!(schedule.interval(), Duration::from_secs(35));
    }

    #[test]
    fn a_zero_interval_is_the_rfc_default() {
        assert_eq!(
            PollSchedule::new(&authorization(0, 900)).interval(),
            DEFAULT_INTERVAL
        );
    }

    #[test]
    fn the_schedule_runs_out_with_the_code() {
        let mut schedule = PollSchedule::new(&authorization(5, 12));
        assert!(schedule.waited(), "7s left");
        assert!(schedule.waited(), "2s left");
        assert!(!schedule.waited(), "out");
    }

    #[test]
    fn the_prefilled_page_is_preferred() {
        let mut auth = authorization(5, 900);
        assert_eq!(auth.page_to_open(), "https://github.com/login/device");
        auth.verification_uri_complete = Some("https://example.com/?code=X".into());
        assert_eq!(auth.page_to_open(), "https://example.com/?code=X");
    }
}
