//! [`ReasoningEffort`] — the provider-neutral reasoning-effort knob.
//!
//! A config parser, a coordinator, or a harness can carry an effort level
//! without pulling in the model-client transport layer; wire code that needs an
//! effort string uses [`ReasoningEffort::as_str`].

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

/// Reasoning effort for the OpenAI Responses API.
///
/// Also the level the terminal's effort indicator draws and the `/effort`
/// command sets, so all three surfaces share one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ReasoningEffort {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    /// Extra-high reasoning for gpt-5.3 / gpt-5.3-codex / gpt-5.4+.
    #[serde(rename = "xhigh")]
    XHigh,
    /// Maximum reasoning for gpt-5.6+ (Sol / Terra / Luna).
    #[serde(rename = "max")]
    Max,
}

impl ReasoningEffort {
    /// The canonical spelling of this level — identical to what `serde`
    /// emits on the wire and what [`FromStr`] accepts back.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The exact inverse of [`Self::as_str`]: the five canonical
    /// spellings, nothing else.
    ///
    /// Separate from [`FromStr`] on purpose, and narrower than it. This
    /// one reads back a value rebon itself wrote — a job record, a
    /// picker id, a session sidecar — where the only strings that can
    /// appear are the ones `as_str` produces, and where accepting
    /// `"HIGH"` or `" high "` would mean accepting a record nothing in
    /// rebon writes. `FromStr` is for what a person types.
    ///
    /// Callers that need a different treatment of an unrecognized value decide
    /// that themselves; this only reports `None`.
    pub fn from_wire_exact(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error from [`ReasoningEffort::from_str`]: the input is not one of
/// the known levels. Carries the trimmed input for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseReasoningEffortError {
    raw: String,
}

impl ParseReasoningEffortError {
    /// The (trimmed) input that failed to parse.
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for ParseReasoningEffortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown reasoning effort {:?} (expected low, medium, high, xhigh, or max)",
            self.raw
        )
    }
}

impl std::error::Error for ParseReasoningEffortError {}

impl FromStr for ReasoningEffort {
    type Err = ParseReasoningEffortError;

    /// Case-insensitive, surrounding whitespace ignored. Anything that
    /// is not exactly one of the five levels after that is an error —
    /// there are no aliases.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            _ => Err(ParseReasoningEffortError {
                raw: trimmed.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ReasoningEffort; 5] = [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ];

    #[test]
    fn parses_all_levels_case_insensitively_and_trimmed() {
        assert_eq!("low".parse::<ReasoningEffort>(), Ok(ReasoningEffort::Low));
        assert_eq!(
            "  Medium\t".parse::<ReasoningEffort>(),
            Ok(ReasoningEffort::Medium)
        );
        assert_eq!("HIGH".parse::<ReasoningEffort>(), Ok(ReasoningEffort::High));
        assert_eq!(
            "xHigh".parse::<ReasoningEffort>(),
            Ok(ReasoningEffort::XHigh)
        );
        assert_eq!("MAX ".parse::<ReasoningEffort>(), Ok(ReasoningEffort::Max));
    }

    #[test]
    fn rejects_unknown_and_empty_inputs() {
        for raw in ["", "   ", "x-high", "x_high", "extreme", "lowest", "hi"] {
            let err = raw.parse::<ReasoningEffort>().expect_err("must not parse");
            assert_eq!(err.raw(), raw.trim());
            assert!(err.to_string().contains("unknown reasoning effort"));
        }
    }

    #[test]
    fn from_wire_exact_is_the_inverse_of_as_str_and_nothing_more() {
        for effort in ALL {
            assert_eq!(
                ReasoningEffort::from_wire_exact(effort.as_str()),
                Some(effort)
            );
        }
        // Narrower than `FromStr` on exactly these two axes, which is
        // why it exists: a wire value rebon wrote is already canonical.
        assert_eq!(ReasoningEffort::from_wire_exact("HIGH"), None);
        assert_eq!(ReasoningEffort::from_wire_exact(" high "), None);
        assert_eq!("HIGH".parse::<ReasoningEffort>(), Ok(ReasoningEffort::High));
        for raw in ["", "med", "x_high", "extra_high", "auto"] {
            assert_eq!(ReasoningEffort::from_wire_exact(raw), None);
        }
    }

    #[test]
    fn as_str_round_trips_and_matches_serde() {
        for effort in ALL {
            assert_eq!(effort.as_str().parse::<ReasoningEffort>(), Ok(effort));
            assert_eq!(effort.to_string(), effort.as_str());
            let json = serde_json::to_string(&effort).unwrap();
            assert_eq!(json, format!("{:?}", effort.as_str()));
        }
    }
}
