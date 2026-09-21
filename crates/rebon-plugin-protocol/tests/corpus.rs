use rebon_plugin_protocol::{CodecError, NdjsonCodec, WireMessage};
use serde_json::{json, value::RawValue, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/manifest.json")
}
fn bytes(source: &Value) -> Vec<u8> {
    if let Some(ascii) = source.get("ascii").and_then(Value::as_str) {
        return ascii.as_bytes().to_vec();
    }
    if let Some(hex) = source.get("hex").and_then(Value::as_str) {
        return hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
    }
    assert_eq!(source["generator"], "padded_request");
    let target = source["body_bytes"].as_u64().unwrap() as usize;
    let sample = r#"{"protocol_version":1,"host_epoch":0,"plugin_id":"p","scope_id":"s","scope_generation":0,"call_id":"padding","message":{"type":"notification","method":"m","payload":""}}"#;
    let padding = target - sample.len();
    let mut body = sample
        .replacen("\"\"}}", &format!("\"{}\"}}}}", "x".repeat(padding)), 1)
        .into_bytes();
    assert_eq!(body.len(), target);
    body.extend_from_slice(match source["ending"].as_str().unwrap() {
        "lf" => b"\n",
        "crlf" => b"\r\n",
        "none" => b"",
        _ => unreachable!(),
    });
    body
}
fn family(error: &CodecError, input: &[u8]) -> &'static str {
    match error {
        CodecError::EmptyFrame => "empty_frame",
        CodecError::FrameTooLarge { .. } => "frame_too_large",
        CodecError::InvalidUtf8(_) => "invalid_utf8",
        CodecError::UnsupportedVersion { .. } => "bad_version",
        CodecError::IncompleteFrame { .. } => "incomplete_frame",
        CodecError::Failed => "poisoned",
        CodecError::InvalidEnvelope(error) => {
            let text = error.to_string();
            if input.starts_with(&[0xef, 0xbb, 0xbf]) {
                "utf8_bom"
            } else if text.contains("duplicate field") {
                "duplicate_field"
            } else if text.contains("unknown field") {
                "unknown_field"
            } else if text.contains("unknown variant `done`") {
                "bad_status"
            } else if text.contains("trailing characters") {
                "trailing_json"
            } else if text.contains("surrogate") || text.contains("hex escape") {
                "invalid_surrogate"
            } else if text.contains("protocol version") || text.contains("expected u32") {
                "bad_version"
            } else if text.contains("safe")
                || text.contains("0 through")
                || text.contains("expected u64")
            {
                "unsafe_integer"
            } else if text.contains("invalid type")
                || text.contains("invalid length")
                || text.contains("missing field")
            {
                "wrong_shape"
            } else {
                "malformed_json"
            }
        }
    }
}
fn normalized(envelope: &rebon_plugin_protocol::WireEnvelope, expected: &Value) -> Value {
    let mut out = serde_json::Map::from_iter([
        ("host_epoch".into(), json!(envelope.identity.host_epoch)),
        ("plugin_id".into(), json!(envelope.identity.plugin_id)),
        ("scope_id".into(), json!(envelope.identity.scope_id)),
        (
            "scope_generation".into(),
            json!(envelope.identity.scope_generation),
        ),
        ("call_id".into(), json!(envelope.identity.call_id)),
    ]);
    let (kind, method, status, payload) = match &envelope.message {
        WireMessage::Request { method, payload } => ("request", Some(method), None, payload),
        WireMessage::Notification { method, payload } => {
            ("notification", Some(method), None, payload)
        }
        WireMessage::Terminal { status, payload } => (
            "terminal",
            None,
            Some(format!("{status:?}").to_lowercase()),
            payload,
        ),
        // A chunk carries neither: it belongs to a call that already named its
        // method, and it has not ended.
        WireMessage::Chunk { payload } => ("chunk", None, None, payload),
    };
    out.insert("message_type".into(), json!(kind));
    if let Some(method) = method {
        out.insert("method".into(), json!(method));
    }
    if let Some(status) = status {
        out.insert("status".into(), json!(status));
    }
    // The payload is compared as the exact text it arrived as. That is the
    // property the differential corpus exists to pin: a number token Rust
    // rounded on the way in would agree with Node here only by accident.
    if expected.get("payload_raw").is_some() {
        out.insert("payload_raw".into(), json!(payload.as_raw()));
    }
    if expected.get("payload_number_raw").is_some() {
        let members: BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(payload.as_raw()).unwrap();
        if let Some(number) = members.get("n") {
            out.insert("payload_number_raw".into(), json!(number.get()));
        }
    }
    if expected.get("payload").is_some() {
        // Duplicate keys resolve when the opaque text is parsed, not before.
        if let Some(value) = payload.to_value().unwrap().get("a") {
            out.insert("payload".into(), json!({"a": value}));
        }
    }
    Value::Object(out)
}
#[test]
fn shared_v1_adversarial_exact_byte_differential_corpus_67_cases_67_hashes_zero_disagreements() {
    let manifest: Value = serde_json::from_slice(&fs::read(fixture()).unwrap()).unwrap();
    let cases = manifest["cases"].as_array().unwrap();
    assert_eq!(manifest["version"], 1);
    assert_eq!(cases.len(), 67);
    let mut hashes = 0;
    // Every decoded case that diverged from the corpus is collected so one run
    // reports all of them, not just the first.
    let mut disagreements: Vec<String> = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let input = bytes(&case["source"]);
        assert_eq!(
            format!("{:x}", Sha256::digest(&input)),
            case["sha256"].as_str().unwrap(),
            "{name} hash"
        );
        hashes += 1;
        let mut codec = NdjsonCodec::default();
        let mut decoded = Vec::new();
        let result: Result<(), CodecError> = (|| {
            let mut offset = 0;
            if let Some(chunks) = case.get("chunks").and_then(Value::as_array) {
                for size in chunks {
                    let end = (offset + size.as_u64().unwrap() as usize).min(input.len());
                    decoded.extend(codec.push(&input[offset..end])?);
                    offset = end;
                }
            }
            if offset < input.len() {
                decoded.extend(codec.push(&input[offset..])?);
            }
            if case.get("finish").and_then(Value::as_bool).unwrap_or(false) {
                codec.finish()?;
            }
            Ok(())
        })();
        if let Some(expected) = case.get("error").and_then(Value::as_str) {
            let error = result.as_ref().expect_err("expected rejection");
            assert_eq!(family(error, &input), expected, "{name}: {error}");
            assert!(matches!(codec.push(b"{}\n"), Err(CodecError::Failed)));
        } else {
            result
                .as_ref()
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            codec.finish().unwrap();
            assert_eq!(
                decoded.len(),
                case["accept"].as_u64().unwrap() as usize,
                "{name}"
            );
            if let Some(expected) = case.get("expected") {
                let actual = normalized(&decoded[0], expected);
                if &actual != expected {
                    disagreements.push(format!(
                        "{name} normalized: expected {expected}, decoded {actual}"
                    ));
                }
            }
        }
    }
    assert_eq!(hashes, 67);
    assert!(
        disagreements.is_empty(),
        "{} case(s) decoded differently than the corpus records: {disagreements:#?}",
        disagreements.len()
    );
}

#[test]
fn opaque_payload_duplicates_are_last_wins_but_typed_duplicates_reject() {
    let value: Value = serde_json::from_str(r#"{"a":1,"a":2}"#).unwrap();
    assert_eq!(value, json!({"a": 2}));
    let mut codec = NdjsonCodec::default();
    assert!(codec.push(br#"{"protocol_version":1,"protocol_version":1,"host_epoch":0,"plugin_id":"p","scope_id":"s","scope_generation":0,"call_id":"c","message":{"type":"notification","method":"m","payload":null}}
"#).is_err());
}
