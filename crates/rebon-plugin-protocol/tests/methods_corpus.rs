//! The Rust half of the shared method-payload corpus.
//!
//! `runtimes/node/plugin-host/test/methods.test.mjs` reads the same file and must reach
//! the same verdict on every case, and the same bracketed code on every
//! refusal. A payload schema that drifts between the languages is a plugin that
//! works against one host and not the other, and the drift is invisible until
//! someone writes the plugin.

use std::{fs, path::PathBuf};

use rebon_plugin_protocol::{
    CommandInvokeRequest, EventDelivery, EventEmitRequest, EventSubscribeRequest,
    EventUnsubscribeRequest, LlmControlRequest, LlmStreamRequest, PluginDrainReport,
    PluginLoadRequest, PluginReadyReport, PluginUnloadRequest, SeatCallRequest, ServiceCallRequest,
    ToolInvokeRequest,
};
use serde_json::Value;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/methods.json")
}

/// Maps a serde rejection onto the shape codes the corpus uses.
///
/// Serde owns these messages, so this reads them rather than inventing codes —
/// the same trade the framing corpus makes for its error families.
fn shape_code(error: &serde_json::Error) -> &'static str {
    let text = error.to_string();
    if text.contains("unknown field") {
        "[UNKNOWN_FIELD]"
    } else if text.contains("missing field") {
        "[MISSING_FIELD]"
    } else {
        "[WRONG_SHAPE]"
    }
}

/// Parses and validates one payload, returning `None` on acceptance.
fn verdict(kind: &str, payload: &Value) -> Option<&'static str> {
    fn check<T, F>(payload: &Value, validate: F) -> Option<&'static str>
    where
        T: serde::de::DeserializeOwned,
        F: FnOnce(&T) -> Result<(), rebon_plugin_protocol::PayloadError>,
    {
        match serde_json::from_value::<T>(payload.clone()) {
            Err(error) => Some(shape_code(&error)),
            Ok(parsed) => validate(&parsed).err().map(|error| error.code()),
        }
    }

    match kind {
        "plugin_load" => check::<PluginLoadRequest, _>(payload, PluginLoadRequest::validate),
        "plugin_ready" => check::<PluginReadyReport, _>(payload, PluginReadyReport::validate),
        "plugin_unload" => check::<PluginUnloadRequest, _>(payload, PluginUnloadRequest::validate),
        "plugin_drain" => check::<PluginDrainReport, _>(payload, PluginDrainReport::validate),
        "service_call" => check::<ServiceCallRequest, _>(payload, ServiceCallRequest::validate),
        "event_subscribe" => {
            check::<EventSubscribeRequest, _>(payload, EventSubscribeRequest::validate)
        }
        "event_unsubscribe" => {
            check::<EventUnsubscribeRequest, _>(payload, EventUnsubscribeRequest::validate)
        }
        "event_deliver" => check::<EventDelivery, _>(payload, EventDelivery::validate),
        "event_emit" => check::<EventEmitRequest, _>(payload, EventEmitRequest::validate),
        "tool_invoke" => check::<ToolInvokeRequest, _>(payload, ToolInvokeRequest::validate),
        "llm_stream" => check::<LlmStreamRequest, _>(payload, LlmStreamRequest::validate),
        "llm_control" => check::<LlmControlRequest, _>(payload, LlmControlRequest::validate),
        "command_invoke" => {
            check::<CommandInvokeRequest, _>(payload, CommandInvokeRequest::validate)
        }
        "seat_call" => check::<SeatCallRequest, _>(payload, SeatCallRequest::validate),
        other => panic!("corpus names an unknown payload kind `{other}`"),
    }
}

#[test]
fn every_shared_method_payload_case_agrees_with_the_rust_schemas() {
    let manifest: Value = serde_json::from_slice(&fs::read(fixture()).unwrap()).unwrap();
    assert_eq!(manifest["version"], 1);
    let cases = manifest["cases"].as_array().unwrap();
    assert!(cases.len() >= 30, "the corpus lost cases");

    let mut names = std::collections::BTreeSet::new();
    let mut kinds = std::collections::BTreeSet::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert!(names.insert(name), "{name} appears twice");
        let kind = case["kind"].as_str().unwrap();
        kinds.insert(kind);

        let accepted = case.get("accept").and_then(Value::as_bool).unwrap_or(false);
        let expected = case.get("code").and_then(Value::as_str);
        assert!(
            accepted ^ expected.is_some(),
            "{name} must be exactly one of accepted or refused"
        );

        match (verdict(kind, &case["payload"]), expected) {
            (None, None) => {}
            (Some(actual), Some(expected)) => assert_eq!(actual, expected, "{name}"),
            (Some(actual), None) => panic!("{name} was refused with {actual} but should pass"),
            (None, Some(expected)) => panic!("{name} passed but should be refused with {expected}"),
        }
    }

    // Every schema the corpus is meant to cover is actually exercised.
    assert_eq!(
        kinds,
        [
            "event_deliver",
            "event_emit",
            "event_subscribe",
            "event_unsubscribe",
            "plugin_drain",
            "plugin_load",
            "plugin_ready",
            "plugin_unload",
            "command_invoke",
            "llm_control",
            "llm_stream",
            "seat_call",
            "service_call",
            "tool_invoke",
        ]
        .into_iter()
        .collect()
    );
}

/// Validation order decides which code a payload with two problems reports, so
/// it is part of the contract rather than an accident of the field order.
#[test]
fn the_corpus_pins_the_order_two_problems_are_reported_in() {
    let manifest: Value = serde_json::from_slice(&fs::read(fixture()).unwrap()).unwrap();
    let ordered: Vec<&str> = manifest["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["name"].as_str().unwrap())
        .filter(|name| name.contains("_before_the_"))
        .collect();
    assert_eq!(
        ordered,
        vec![
            "load_reports_the_plugin_id_before_the_entry",
            "subscribe_reports_the_subscription_before_the_topic",
        ]
    );
}
