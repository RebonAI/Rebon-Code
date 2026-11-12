//! The model's view of a finished shell command: its output as plain text.
//!
//! The shell tools return a JSON object, and the engine's generic projection
//! used to hand that object to the model as it was: the command and
//! description the model had just written, flags that were `false`, and both
//! streams with every newline and quote escaped. About a third of each Bash
//! result was that overhead, and a result is resent with every later request
//! of the session. The model now reads the output the way a terminal shows
//! it, followed only by what is unusual about the run: a nonzero exit code,
//! the note a timeout, signal or terminal stop leaves, the background shell's
//! id and output path, and any field this module does not know, so nothing a
//! newer field carries is lost.

use rebon_tools_core::shell_stream_order::{interleave, stream_display_lines, STREAM_ORDER_KEY};
use serde_json::{Map, Value};

/// Fields folded into the text, or that only echo the call's own input.
const FOLDED: &[&str] = &[
    "stdout",
    "stderr",
    "exitCode",
    "command",
    "description",
    "dangerouslyDisableSandbox",
    "note",
    "shellId",
    "outputPath",
    "timedOut",
    "interrupted",
    STREAM_ORDER_KEY,
];

/// Shown when a command printed nothing and nothing about it was unusual.
const NO_OUTPUT: &str = "(no output)";

/// The text the model receives for a shell tool's result, or `None` when the
/// value is not a shell result this module recognises.
pub(crate) fn shell_result_text(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if !object.contains_key("stdout") && !object.contains_key("shellId") {
        return None;
    }
    let stdout = string_field(object, "stdout");
    let stderr = string_field(object, "stderr");
    let order = object.get(STREAM_ORDER_KEY).and_then(Value::as_str);
    let body = output_body(stdout, stderr, order);

    let mut footer = Vec::new();
    if let Some(code) = object
        .get("exitCode")
        .filter(|code| code.as_i64() != Some(0))
    {
        if !code.is_null() {
            footer.push(format!("Exit code {code}"));
        }
    }
    let note = string_field(object, "note");
    if is_true(object, "interrupted") && note.is_empty() {
        footer.push("Interrupted".to_owned());
    }
    if is_true(object, "timedOut") && note.is_empty() {
        footer.push("Timed out".to_owned());
    }
    if !note.is_empty() {
        footer.push(note.to_owned());
    }
    for key in ["shellId", "outputPath"] {
        let field = string_field(object, key);
        if !field.is_empty() {
            footer.push(format!("{key}: {field}"));
        }
    }
    for (key, field) in object {
        if FOLDED.contains(&key.as_str()) || is_unremarkable(field) {
            continue;
        }
        let shown = match field {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        footer.push(format!("{key}: {shown}"));
    }

    let text = match (body.is_empty(), footer.is_empty()) {
        (true, true) => NO_OUTPUT.to_owned(),
        (true, false) => footer.join("\n"),
        (false, true) => body,
        (false, false) => format!("{body}\n\n{}", footer.join("\n")),
    };
    Some(text)
}

/// Both streams as one text: in arrival order when the tool recorded how
/// they interleaved, otherwise stdout followed by stderr.
fn output_body(stdout: &str, stderr: &str, order: Option<&str>) -> String {
    if let Some(sketch) = order {
        let out = stream_display_lines(stdout);
        let err = stream_display_lines(stderr);
        if let Some(lines) = interleave(sketch, &out, &err) {
            return lines.join("\n");
        }
    }
    let stdout = stdout.trim_end_matches('\n');
    let stderr = stderr.trim_end_matches('\n');
    match (stdout.is_empty(), stderr.is_empty()) {
        (_, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> &'a str {
    object.get(key).and_then(Value::as_str).unwrap_or("")
}

fn is_true(object: &Map<String, Value>, key: &str) -> bool {
    object.get(key).and_then(Value::as_bool) == Some(true)
}

/// A field whose value says nothing: absent-like, `false`, or empty.
fn is_unremarkable(field: &Value) -> bool {
    match field {
        Value::Null | Value::Bool(false) => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(value: Value) -> String {
        shell_result_text(&value).expect("a shell result")
    }

    #[test]
    fn a_plain_success_is_its_output_and_nothing_else() {
        let value = json!({
            "stdout": "a\nb\n",
            "stderr": "",
            "exitCode": 0,
            "command": "printf 'a\\nb\\n'",
            "description": "Print two lines",
            "dangerouslyDisableSandbox": false,
            "timedOut": false,
            "interrupted": false,
        });
        assert_eq!(text(value), "a\nb");
    }

    #[test]
    fn a_failure_keeps_stderr_and_names_the_exit_code() {
        let value = json!({
            "stdout": "ran 3 tests\n",
            "stderr": "FAILED: test_x\n[rebon: exit 1: python3 -m unittest]\n",
            "exitCode": 1,
            "command": "python3 -m unittest",
        });
        assert_eq!(
            text(value),
            "ran 3 tests\nFAILED: test_x\n[rebon: exit 1: python3 -m unittest]\n\nExit code 1"
        );
    }

    #[test]
    fn interleaved_streams_come_back_in_arrival_order() {
        let value = json!({
            "stdout": "test a ... ok\ntest b ... ok\n",
            "stderr": "Compiling x\nFinished\n",
            "exitCode": 0,
            STREAM_ORDER_KEY: "e2,o2",
        });
        assert_eq!(
            text(value),
            "Compiling x\nFinished\ntest a ... ok\ntest b ... ok"
        );
    }

    #[test]
    fn a_sketch_that_does_not_fit_falls_back_to_stdout_then_stderr() {
        let value = json!({
            "stdout": "out\n",
            "stderr": "err\n",
            "exitCode": 0,
            STREAM_ORDER_KEY: "e5,o5",
        });
        assert_eq!(text(value), "out\nerr");
    }

    #[test]
    fn a_command_moved_to_the_background_says_where_it_went() {
        let value = json!({
            "stdout": "partial\n",
            "stderr": "",
            "exitCode": null,
            "timedOut": true,
            "backgrounded": true,
            "status": "running",
            "note": "Still running when its 60000 ms timeout passed, so it was not killed.",
            "shellId": "sh_1",
            "outputPath": "/tmp/out.log",
            "command": "sleep 100",
        });
        assert_eq!(
            text(value),
            "partial\n\nStill running when its 60000 ms timeout passed, so it was not killed.\n\
             shellId: sh_1\noutputPath: /tmp/out.log\nbackgrounded: true\nstatus: running"
        );
    }

    #[test]
    fn silence_is_said_out_loud() {
        let value = json!({ "stdout": "", "stderr": "", "exitCode": 0, "command": "true" });
        assert_eq!(text(value), NO_OUTPUT);
    }

    #[test]
    fn a_field_it_does_not_know_still_reaches_the_model() {
        let value = json!({
            "stdout": "x",
            "stderr": "",
            "exitCode": 0,
            "signal": 9,
            "sandboxNotice": "network blocked",
        });
        assert_eq!(
            text(value),
            "x\n\nsandboxNotice: network blocked\nsignal: 9"
        );
    }

    #[test]
    fn a_value_that_is_not_a_shell_result_is_left_to_the_engine() {
        assert_eq!(shell_result_text(&json!({ "matches": [] })), None);
        assert_eq!(shell_result_text(&json!("text")), None);
    }

    #[test]
    fn the_text_is_much_smaller_than_the_json_it_replaces() {
        let value = json!({
            "stdout": "def f():\n    return \"x\"\n",
            "stderr": "",
            "exitCode": 0,
            "command": "cat src/f.py",
            "description": "Show f.py",
            "dangerouslyDisableSandbox": false,
            "timedOut": false,
            "interrupted": false,
        });
        let json = serde_json::to_string(&value).unwrap();
        assert!(text(value).len() * 3 < json.len());
    }
}
