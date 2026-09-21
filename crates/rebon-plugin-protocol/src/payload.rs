//! The opaque part of a message, kept as the exact JSON text it arrived as.
//!
//! Payload contents belong to a method's own contract, not to this crate, so
//! the wire layer must hand them on **unchanged**. `serde_json::Value` cannot do
//! that: a number that does not fit `f64` is rounded on the way in, and the
//! token it was written as is gone by the time anyone could object. A peer that
//! can hold big integers and refuses to re-encode them implicitly has to find the
//! same promise here, or the two disagree about frames they are required to agree
//! on.
//!
//! The obvious lever — `serde_json`'s `arbitrary_precision` feature — is a trap
//! at workspace scale. Cargo unifies features across a build graph, so enabling
//! it here enables it for every crate compiled alongside this one, and with it
//! on, `serde_json` yields numbers as maps through `deserialize_any`. That is
//! exactly the path `#[serde(tag = "...")]` and `#[serde(flatten)]` take, so
//! every internally-tagged enum or flattened struct with a numeric field stops
//! deserializing — and the failure surfaces in some unrelated crate rather than
//! in the one that turned the feature on.
//!
//! `RawValue` carries the same guarantee without touching global number
//! semantics. It has one constraint of its own, and it is the same constraint
//! from the other side: it cannot be captured through serde's `Content`
//! buffering, which is why [`crate::WireEnvelope`] and [`crate::WireMessage`]
//! implement `Serialize`/`Deserialize` by hand instead of deriving them.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{value::RawValue, Value};

/// One message's opaque payload.
///
/// Equality is over the exact JSON text, not over a parsed structure: two
/// payloads are the same payload when they are the same bytes. For a wire
/// contract that is the useful question, and it is the only one that can be
/// answered without parsing — which is what this type exists to avoid.
#[derive(Clone, Debug)]
pub struct Payload(Box<RawValue>);

impl Default for Payload {
    /// A missing payload is the JSON literal `null`, not an absent value: a
    /// message shape with an optional payload still has to say something on the
    /// wire, and `null` is what "nothing" is spelled as.
    fn default() -> Self {
        Self::null()
    }
}

impl Payload {
    /// The JSON literal `null`, which is what a payload-less message carries.
    pub fn null() -> Self {
        Self(RawValue::from_string("null".to_owned()).expect("`null` is a valid JSON value"))
    }

    pub fn from_raw(raw: Box<RawValue>) -> Self {
        Self(raw)
    }

    /// The payload's exact JSON text, as received or as it will be sent.
    pub fn as_raw(&self) -> &str {
        self.0.get().trim()
    }

    pub fn into_raw(self) -> Box<RawValue> {
        self.0
    }

    pub fn is_null(&self) -> bool {
        self.as_raw() == "null"
    }

    /// A parsed view, for a caller that has a schema for this method.
    ///
    /// Parsing is where exactness ends: a number outside `f64` is rounded here,
    /// which is why this is a deliberate call and not how the payload is stored.
    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(self.0.get())
    }
}

impl PartialEq for Payload {
    fn eq(&self, other: &Self) -> bool {
        self.as_raw() == other.as_raw()
    }
}

impl Eq for Payload {}

impl fmt::Display for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_raw())
    }
}

/// Convenience for callers that build a payload structurally. A `Value` holds
/// only finite numbers, so it always re-encodes.
impl From<Value> for Payload {
    fn from(value: Value) -> Self {
        Self(
            serde_json::value::to_raw_value(&value)
                .expect("a serde_json Value is always encodable"),
        )
    }
}

impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn null_is_the_literal_and_is_recognised() {
        assert_eq!(Payload::null().as_raw(), "null");
        assert!(Payload::null().is_null());
        assert!(!Payload::from(json!(0)).is_null());
        assert!(!Payload::from(json!("null")).is_null());
    }

    /// The whole point: a token `f64` cannot hold survives untouched.
    #[test]
    fn a_number_outside_f64_precision_keeps_its_token() {
        let raw = RawValue::from_string("0.0000000000000000001".to_owned()).unwrap();
        let payload = Payload::from_raw(raw);
        assert_eq!(payload.as_raw(), "0.0000000000000000001");
        // …and it is still there after a round trip through the wire.
        let encoded = serde_json::to_string(&payload).unwrap();
        assert_eq!(encoded, "0.0000000000000000001");
        let decoded: Payload = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, payload);
        // Parsing is where the exactness is spent, and only on request.
        assert_ne!(payload.to_value().unwrap().to_string(), payload.as_raw());
    }

    #[test]
    fn equality_is_over_the_bytes() {
        let compact: Payload = serde_json::from_str(r#"{"a":1}"#).unwrap();
        let spaced: Payload = serde_json::from_str(r#"{ "a" : 1 }"#).unwrap();
        assert_ne!(compact, spaced);
        assert_eq!(compact.to_value().unwrap(), spaced.to_value().unwrap());
    }

    #[test]
    fn a_value_round_trips_through_the_canonical_encoding() {
        let payload = Payload::from(json!({"b": 2, "a": [1, true, null]}));
        let decoded: Payload = serde_json::from_str(payload.as_raw()).unwrap();
        assert_eq!(decoded, payload);
        assert_eq!(
            decoded.to_value().unwrap(),
            json!({"a": [1, true, null], "b": 2})
        );
    }

    /// Last-wins duplicate keys are a property of parsing, so the raw text keeps
    /// both and a caller that parses sees serde_json's answer.
    #[test]
    fn duplicate_keys_survive_as_text_and_resolve_on_parse() {
        let payload: Payload = serde_json::from_str(r#"{"a":1,"a":2}"#).unwrap();
        assert_eq!(payload.as_raw(), r#"{"a":1,"a":2}"#);
        assert_eq!(payload.to_value().unwrap(), json!({"a": 2}));
    }

    #[test]
    fn invalid_json_never_becomes_a_payload() {
        assert!(serde_json::from_str::<Payload>("{").is_err());
        assert!(serde_json::from_str::<Payload>("nul").is_err());
        assert!(serde_json::from_str::<Payload>("1 2").is_err());
    }
}
