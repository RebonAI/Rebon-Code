//! The checked-in generated files have to match a fresh run.
//!
//! Without this the generator is advice: someone renames a Rust field, forgets
//! to regenerate, and the browser client goes on compiling against a shape the
//! server stopped sending — the exact silent drift the hand-written mirror had.

use std::path::{Path, PathBuf};

use rebon_schema_gen::{artefacts, REGENERATE_COMMAND};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives two levels under the repository root")
        .to_path_buf()
}

#[test]
fn generated_files_are_current() {
    let root = repo_root();
    for (relative, expected) in artefacts() {
        let path = root.join(relative);
        let actual = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("{relative} is missing ({error}); run `{REGENERATE_COMMAND}`")
        });
        // The working tree may be CRLF on Windows even where the index is LF,
        // so compare on the content rather than on the checkout's line
        // endings; `.gitattributes` pins these two files to LF for the same
        // reason, and this keeps a stale attributes file from turning a real
        // drift report into a line-ending complaint.
        let normalize = |text: &str| text.replace("\r\n", "\n");
        let (actual, expected) = (normalize(&actual), normalize(&expected));
        if actual != expected {
            panic!(
                "{relative} is out of date — run `{REGENERATE_COMMAND}`\n{}",
                first_difference(&actual, &expected)
            );
        }
    }
}

/// The first line that differs, rather than both files.
///
/// A whole generated file in the failure output buries the one line that
/// changed; the point of the message is to say what drifted and how to fix it.
fn first_difference(actual: &str, expected: &str) -> String {
    let mut actual_lines = actual.lines();
    let mut expected_lines = expected.lines();
    let mut line = 0;
    loop {
        line += 1;
        match (actual_lines.next(), expected_lines.next()) {
            (None, None) => return "the files differ only in trailing newlines".to_string(),
            (a, e) if a == e => continue,
            (a, e) => {
                return format!(
                    "first difference at line {line}\n  on disk:    {}\n  generated:  {}",
                    a.unwrap_or("<end of file>"),
                    e.unwrap_or("<end of file>"),
                )
            }
        }
    }
}

#[test]
fn generation_is_reproducible() {
    let first = artefacts();
    let second = artefacts();
    assert_eq!(first, second, "two runs must produce the same bytes");
}

/// The generated TypeScript writes an optional field as `field?: T`, with no
/// `| null`. That is only honest while every optional field really is left out
/// rather than sent as `null`, so hold it here: a value whose `Option`s are all
/// `None` must serialize without a single null.
///
/// `DiffContent::old_text` is the documented exception and carries
/// `schemars(required, extend(...))` so the generated type says
/// `oldText: string | null`; it is checked separately below.
#[test]
fn optional_fields_are_omitted_not_nulled() {
    use rebon_proto::types as proto;

    let empty: Vec<serde_json::Value> = vec![
        serde_json::to_value(proto::AgentCapabilities::default()).unwrap(),
        serde_json::to_value(proto::PromptCapabilities::default()).unwrap(),
        serde_json::to_value(proto::McpCapabilities::default()).unwrap(),
        serde_json::to_value(proto::SessionCapabilities::default()).unwrap(),
        serde_json::to_value(proto::AuthMethod::default()).unwrap(),
        serde_json::to_value(proto::SessionListResult::default()).unwrap(),
        serde_json::to_value(proto::JsonRpcError::internal_error("boom")).unwrap(),
        serde_json::to_value(proto::SessionInfo {
            session_id: "s".into(),
            cwd: "/tmp".into(),
            title: None,
            updated_at: None,
            meta: None,
        })
        .unwrap(),
        serde_json::to_value(proto::SessionNewResult {
            session_id: "s".into(),
            config_options: None,
            slash_commands: None,
        })
        .unwrap(),
        serde_json::to_value(proto::RequestPermissionParams {
            session_id: "s".into(),
            tool_call: rebon_types::ToolCallReference {
                tool_call_id: "t".into(),
            },
            options: Vec::new(),
            title: None,
            message: None,
            tool_name: None,
            tool_input: None,
            metadata: None,
        })
        .unwrap(),
        serde_json::to_value(rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "hi".into(),
            annotations: None,
        }))
        .unwrap(),
        serde_json::to_value(rebon_types::ToolCallLocation {
            path: "/tmp/a".into(),
            line: None,
        })
        .unwrap(),
        serde_json::to_value(rebon_types::SessionUpdate::ToolCall {
            tool_call_id: "t".into(),
            title: "t".into(),
            kind: rebon_types::ToolKind::Read,
            status: rebon_types::ToolCallStatus::Pending,
            content: None,
            locations: None,
            raw_input: None,
            raw_output: None,
        })
        .unwrap(),
    ];
    for value in empty {
        assert!(
            !contains_null(&value),
            "an all-`None` value serialized a null, so `field?: T` is no longer \
             the right generated shape: {value}"
        );
    }

    // The one field that does send null, and does say so in its type.
    let diff = serde_json::to_value(rebon_types::DiffContent {
        path: "/tmp/a".into(),
        old_text: None,
        new_text: "x".into(),
    })
    .unwrap();
    assert_eq!(diff["oldText"], serde_json::Value::Null);
}

fn contains_null(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Array(items) => items.iter().any(contains_null),
        serde_json::Value::Object(fields) => fields.values().any(contains_null),
        _ => false,
    }
}
