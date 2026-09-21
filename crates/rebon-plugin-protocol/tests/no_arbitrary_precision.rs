//! A guard against re-enabling `serde_json`'s `arbitrary_precision`.
//!
//! This crate needs a number's exact token, and `arbitrary_precision` is the
//! obvious way to get one. It is also a workspace-wide hazard: Cargo unifies
//! features across a build graph, so enabling it here enables it for every
//! crate compiled alongside this one, and with it on `serde_json` yields
//! numbers as maps through `deserialize_any` — the path `#[serde(tag = "...")]`
//! and `#[serde(flatten)]` take. This workspace declares around 45 such enums,
//! including the model wire types, and any of them carrying a number stops
//! deserializing.
//!
//! It got as far as `cargo test --workspace` failing before anyone noticed,
//! because CI runs per-crate `-p` tests and each of those has its own feature
//! graph. So the check lives here, in the crate that would cause it, and fails
//! at the moment the feature comes back rather than in some unrelated crate.
//!
//! `src/payload.rs` explains what replaced it.

use serde::{Deserialize, Serialize};

/// The exact shape that breaks: an internally-tagged enum with a float field.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Shape {
    Point { x: f64, y: f64 },
    Count { n: u64 },
}

#[test]
fn internally_tagged_enums_with_numbers_still_round_trip() {
    for shape in [
        Shape::Point { x: 1.5, y: -2.25 },
        Shape::Count {
            n: 9_007_199_254_740_991,
        },
    ] {
        let encoded = serde_json::to_string(&shape).unwrap();
        let decoded: Shape = serde_json::from_str(&encoded).unwrap_or_else(|error| {
            panic!(
                "`{encoded}` failed to deserialize ({error}). If this crate — or anything in its \
                 build graph — enabled serde_json's `arbitrary_precision`, that is the cause: it \
                 makes every number arrive as a map on the buffered path internally-tagged enums \
                 use. See src/payload.rs."
            )
        });
        assert_eq!(decoded, shape);
    }
}

/// The same hazard through the other buffering construct.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Flattened {
    name: String,
    #[serde(flatten)]
    inner: Inner,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Inner {
    ratio: f64,
}

#[test]
fn flattened_structs_with_numbers_still_round_trip() {
    let value = Flattened {
        name: "r".into(),
        inner: Inner { ratio: 0.5 },
    };
    let encoded = serde_json::to_string(&value).unwrap();
    let decoded: Flattened = serde_json::from_str(&encoded)
        .unwrap_or_else(|error| panic!("`{encoded}` failed to deserialize ({error})"));
    assert_eq!(decoded, value);
}

/// And the property the feature was wanted for, still held without it: a token
/// `f64` cannot represent survives a decode/encode round trip unchanged.
#[test]
fn payload_tokens_survive_without_the_feature() {
    use rebon_plugin_protocol::{NdjsonCodec, WireMessage};

    let frame = concat!(
        r#"{"protocol_version":1,"host_epoch":7,"plugin_id":"p","scope_id":"s","#,
        r#""scope_generation":3,"call_id":"c","message":{"type":"notification","#,
        r#""method":"m","payload":{"n":0.10000000000000001}}}"#,
        "\n"
    );
    let mut codec = NdjsonCodec::default();
    let decoded = codec.push(frame.as_bytes()).unwrap();
    codec.finish().unwrap();

    let WireMessage::Notification { payload, .. } = &decoded[0].message else {
        panic!("expected a notification");
    };
    assert_eq!(payload.as_raw(), r#"{"n":0.10000000000000001}"#);
    assert_eq!(codec.encode(&decoded[0]).unwrap(), frame.as_bytes());
}
