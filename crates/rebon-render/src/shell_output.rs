//! Body lines for background-shell launch tools (`Bash`, `PowerShell`) and
//! management tools (`ShellOutput`, `ShellStop`).
//!
//! Turns their JSON results into human-readable lines — a launch confirmation
//! or a status line followed by the actual shell output text — instead of the
//! generic `key=value` map dump. One formatter for the streaming path and the
//! committed one, so a result reads the same in every transcript.

use std::collections::HashMap;

use serde_json::Value;

/// Max chars of a command shown per line when listing shells.
const LIST_COMMAND_MAX_CHARS: usize = 80;

/// A tool result object in either of the two shapes render inputs use:
/// `StreamingToolUse.raw_output` holds a `HashMap`, committed transcript
/// rows hold a `serde_json::Value` object.
#[derive(Clone, Copy)]
enum ResultObject<'a> {
    Map(&'a serde_json::Map<String, Value>),
    Hash(&'a HashMap<String, Value>),
}

impl<'a> ResultObject<'a> {
    fn get(self, key: &str) -> Option<&'a Value> {
        match self {
            Self::Map(map) => map.get(key),
            Self::Hash(map) => map.get(key),
        }
    }
}

/// Build body lines for background-shell launch and management results held as
/// the `HashMap` form used by `StreamingToolUse.raw_output`.
///
/// Returns `None` when the tool/result pair does not match a background-shell
/// response shape, so callers can fall back to their generic rendering.
pub fn shell_management_body_lines(
    tool_name: &str,
    raw_output: &HashMap<String, Value>,
) -> Option<Vec<String>> {
    shell_management_body_lines_impl(tool_name, ResultObject::Hash(raw_output))
}

/// [`shell_management_body_lines`] for a `serde_json::Value` result
/// (committed transcript rows hold this form).
pub fn shell_management_body_lines_from_value(
    tool_name: &str,
    raw_output: &Value,
) -> Option<Vec<String>> {
    let object = raw_output.as_object()?;
    shell_management_body_lines_impl(tool_name, ResultObject::Map(object))
}

/// Header summary override for `ShellOutput` / `ShellStop`: the command
/// that originally started the shell (echoed back in the tool result),
/// which reads far better than the opaque shellId. Returns `None` while
/// no result is available yet — or for older transcripts whose results
/// carry no `command` — so callers keep their input-based summary.
pub fn shell_management_header_summary(
    tool_name: &str,
    raw_output: &HashMap<String, Value>,
) -> Option<String> {
    shell_management_header_summary_impl(tool_name, ResultObject::Hash(raw_output))
}

/// [`shell_management_header_summary`] for a `serde_json::Value` result.
pub fn shell_management_header_summary_from_value(
    tool_name: &str,
    raw_output: &Value,
) -> Option<String> {
    let object = raw_output.as_object()?;
    shell_management_header_summary_impl(tool_name, ResultObject::Map(object))
}

fn shell_management_header_summary_impl(
    tool_name: &str,
    result: ResultObject<'_>,
) -> Option<String> {
    if !matches!(tool_name, "ShellOutput" | "ShellStop") {
        return None;
    }
    result
        .get("command")
        .and_then(Value::as_str)
        .map(flatten_command)
        .filter(|command| !command.is_empty())
}

fn shell_management_body_lines_impl(
    tool_name: &str,
    result: ResultObject<'_>,
) -> Option<Vec<String>> {
    match tool_name {
        "Bash" | "PowerShell" => {
            result
                .get("shellId")
                .and_then(Value::as_str)
                .filter(|shell_id| !shell_id.is_empty())?;
            if result.get("tool").and_then(Value::as_str) != Some(tool_name) {
                return None;
            }
            result.get("startedAtMs").and_then(Value::as_u64)?;
            Some(vec!["Background shell launched".to_string()])
        }
        "ShellOutput" => {
            if let Some(shells) = result.get("shells").and_then(Value::as_array) {
                return Some(shell_list_lines(shells));
            }
            let status = result.get("status").and_then(Value::as_str)?;
            let (output, stderr) = stream_texts(result);
            let mut lines = vec![shell_output_status_line(
                status,
                result,
                // Presence is still judged on the raw strings, never on the
                // interleaved view: a poll that returned only whitespace
                // advanced the cursor and must not read as "no new output".
                output.is_empty() && stderr.is_empty(),
            )];
            // A `streamOrder` sketch replays the order the lines actually
            // arrived in; without one, keep the historical stdout-then-stderr
            // concatenation.
            let sketch = result
                .get(rebon_tools_core::shell_stream_order::STREAM_ORDER_KEY)
                .and_then(Value::as_str);
            match interleaved_shell_streams(Some(&output), Some(&stderr), sketch) {
                Some(merged) => lines.extend(text_block_lines(&merged)),
                None => {
                    lines.extend(text_block_lines(&output));
                    lines.extend(text_block_lines(&stderr));
                }
            }
            Some(lines)
        }
        "ShellStop" => {
            let status = result.get("status").and_then(Value::as_str)?;
            Some(vec![shell_stop_status_line(status, result)])
        }
        _ => None,
    }
}

/// One line per shell for the no-`shellId` listing call.
fn shell_list_lines(shells: &[Value]) -> Vec<String> {
    if shells.is_empty() {
        return vec!["no background shells".to_string()];
    }
    shells
        .iter()
        .map(|shell| {
            let shell_id = shell
                .get("shellId")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let status = shell.get("status").and_then(Value::as_str).unwrap_or("?");
            let exit_code = shell.get("exitCode").and_then(Value::as_i64);
            let mut line = format!("{shell_id} · {}", status_label(status, exit_code));
            if let Some(command) = shell
                .get("command")
                .and_then(Value::as_str)
                .map(flatten_command)
                .filter(|command| !command.is_empty())
            {
                line.push_str(" · ");
                line.push_str(&command);
            }
            line
        })
        .collect()
}

/// Leading status line for an output poll. Always first so it survives
/// compact previews that keep only the first few body lines.
fn shell_output_status_line(status: &str, result: ResultObject<'_>, no_new_output: bool) -> String {
    let mut line = status_label(status, result.get("exitCode").and_then(Value::as_i64));
    if let Some(error) = result
        .get("error")
        .and_then(Value::as_str)
        .map(first_line)
        .filter(|error| !error.is_empty())
    {
        line.push_str(&format!(" · {error}"));
    }
    if no_new_output {
        line.push_str(" · no new output");
    }
    if flag(result, "hasMore") {
        line.push_str(" · more output buffered");
    }
    if flag(result, "cursorTruncated") {
        line.push_str(" · earlier output dropped");
    }
    line
}

fn shell_stop_status_line(status: &str, result: ResultObject<'_>) -> String {
    let mut line = status_label(status, result.get("exitCode").and_then(Value::as_i64));
    if flag(result, "alreadyCompleted") {
        line.push_str(" · was already finished");
    } else if flag(result, "alreadyRequested") {
        line.push_str(" · stop already requested");
    }
    line
}

/// `exited (code 0)` / `timed out` / raw status for anything unknown.
fn status_label(status: &str, exit_code: Option<i64>) -> String {
    let label = match status {
        "timed_out" => "timed out",
        other => other,
    };
    match exit_code {
        Some(code) => format!("{label} (code {code})"),
        None => label.to_string(),
    }
}

/// New output text per stream: prefers the merged `output` (stdout) and
/// `stderr` strings, falling back to grouping the per-chunk
/// `events[]` array (which older sessions persisted instead) by stream.
///
/// Presence is judged on the raw strings — a poll that returned only
/// whitespace still advanced the cursor and must not read as
/// "no new output".
fn stream_texts(result: ResultObject<'_>) -> (String, String) {
    let output = result.get("output").and_then(Value::as_str);
    let stderr = result.get("stderr").and_then(Value::as_str);
    if output.is_some() || stderr.is_some() {
        return (
            output.unwrap_or_default().to_string(),
            stderr.unwrap_or_default().to_string(),
        );
    }
    let mut from_events_output = String::new();
    let mut from_events_stderr = String::new();
    if let Some(events) = result.get("events").and_then(Value::as_array) {
        for event in events {
            let Some(text) = event.get("text").and_then(Value::as_str) else {
                continue;
            };
            if event.get("stream").and_then(Value::as_str) == Some("stderr") {
                from_events_stderr.push_str(text);
            } else {
                from_events_output.push_str(text);
            }
        }
    }
    (from_events_output, from_events_stderr)
}

/// Rebuild the interleaved shell body from the two per-stream result
/// strings, using the `streamOrder` sketch the Bash/PowerShell tools
/// record alongside them.
///
/// Returns `None` — meaning "keep concatenating stdout then stderr" —
/// whenever the sketch is absent (an older session, or a run where it
/// carried no information), or where it no longer accounts for the
/// strings it is handed (a truncated stream, a corrupted field).
/// `shell_stream_order::interleave` enforces that last check: zipping
/// mismatched streams would scatter output into an order that never
/// happened, which is worse than the concatenation it replaces.
///
/// One implementation of the ordering, so every surface renders the same
/// order.
pub fn interleaved_shell_streams(
    stdout: Option<&str>,
    stderr: Option<&str>,
    stream_order: Option<&str>,
) -> Option<String> {
    let sketch = stream_order?;
    // `stream_display_lines` is the SHARED splitting rule — the same one
    // the tools count with when building a sketch. Using anything else
    // here (a bare `split('\n')`, say) would disagree about the trailing
    // segment of text ending in a newline and send every background-shell
    // sketch down the fallback path.
    let merged = rebon_tools_core::shell_stream_order::interleave(
        sketch,
        &sketch_lines(stdout),
        &sketch_lines(stderr),
    )?;
    Some(merged.join("\n"))
}

fn sketch_lines(text: Option<&str>) -> Vec<&str> {
    text.map(rebon_tools_core::shell_stream_order::stream_display_lines)
        .unwrap_or_default()
}

/// Split a stream's text into display lines, dropping trailing blank
/// lines (a trailing newline should not cost a preview slot).
fn text_block_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.trim_end().to_string())
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn flag(result: ResultObject<'_>, key: &str) -> bool {
    result.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// Flatten a (possibly multi-line) command for a one-line listing entry.
fn flatten_command(command: &str) -> String {
    let flattened = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flattened.chars();
    let truncated: String = chars.by_ref().take(LIST_COMMAND_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}\u{2026}")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lines_for(tool_name: &str, raw_output: Value) -> Option<Vec<String>> {
        shell_management_body_lines_from_value(tool_name, &raw_output)
    }

    /// cargo writes progress (`Compiling`, `Finished`) to stderr and only
    /// results to stdout, so concatenating the streams renders the compile
    /// banner AFTER the test summary. The sketch is what puts it back.
    #[test]
    fn interleaved_shell_streams_restores_cargo_arrival_order() {
        let stdout = ["running 1 test", "test result: ok."].join("\n");
        let stderr = ["Compiling rebon-tool v0.20.0", "Finished test profile"].join("\n");
        let merged = interleaved_shell_streams(Some(&stdout), Some(&stderr), Some("e2,o2"))
            .expect("counts match the sketch");
        assert_eq!(
            merged,
            [
                "Compiling rebon-tool v0.20.0",
                "Finished test profile",
                "running 1 test",
                "test result: ok.",
            ]
            .join("\n")
        );
    }

    #[test]
    fn interleaved_shell_streams_replays_alternating_runs() {
        let stdout = ["a", "b", "c"].join("\n");
        let stderr = ["X", "Y"].join("\n");
        let merged =
            interleaved_shell_streams(Some(&stdout), Some(&stderr), Some("o1,e1,o1,e1,o1"))
                .expect("counts match the sketch");
        assert_eq!(merged, ["a", "X", "b", "Y", "c"].join("\n"));
    }

    /// Every one of these must fall back to concatenation rather than zip
    /// the streams into an order that never happened.
    #[test]
    fn interleaved_shell_streams_falls_back_on_anything_untrustworthy() {
        let stdout = ["a", "b"].join("\n");
        // No sketch at all — an older session.
        assert_eq!(
            interleaved_shell_streams(Some(&stdout), Some("X"), None),
            None
        );
        // Sketch accounts for more stdout lines than exist (truncated stream).
        assert_eq!(
            interleaved_shell_streams(Some(&stdout), Some("X"), Some("e1,o5")),
            None
        );
        // Corrupted sketches.
        for bad in ["o1,o1", "garbage", "", "o0,e1"] {
            assert_eq!(
                interleaved_shell_streams(Some(&stdout), Some("X"), Some(bad)),
                None,
                "sketch {bad:?}"
            );
        }
    }

    /// An empty stream joins to `""`, which naive splitting would count as
    /// one blank line and knock the sketch's totals out of alignment.
    #[test]
    fn interleaved_shell_streams_counts_an_empty_stream_as_zero_lines() {
        // "e1,o1" needs one line on each side; stderr is empty, so fall back.
        assert_eq!(
            interleaved_shell_streams(Some("only out"), Some(""), Some("e1,o1")),
            None
        );
        // A sketch describing only the non-empty side does line up.
        assert_eq!(
            interleaved_shell_streams(Some("a\nb"), Some("X"), Some("e1,o2")).as_deref(),
            Some("X\na\nb")
        );
    }

    #[test]
    fn non_shell_tools_fall_through() {
        assert_eq!(lines_for("Read", json!({ "status": "exited" })), None);
        assert_eq!(lines_for("ShellOutput", json!({ "bytes": 5 })), None);
        assert_eq!(lines_for("ShellOutput", json!("not an object")), None);
    }

    #[test]
    fn background_shell_launch_shows_confirmation() {
        for tool_name in ["Bash", "PowerShell"] {
            let lines = lines_for(
                tool_name,
                json!({
                    "shellId": "sh_1",
                    "tool": tool_name,
                    "command": "cargo test",
                    "status": "running",
                    "startedAtMs": 42,
                    "completedAtMs": null,
                    "timeoutMs": null,
                    "exitCode": null
                }),
            )
            .unwrap();
            assert_eq!(lines, vec!["Background shell launched"]);
        }
    }

    #[test]
    fn foreground_or_mismatched_shell_results_fall_through() {
        assert_eq!(
            lines_for(
                "Bash",
                json!({
                    "stdout": "done",
                    "stderr": "",
                    "exitCode": 0,
                    "command": "cargo test"
                })
            ),
            None
        );
        assert_eq!(
            lines_for(
                "PowerShell",
                json!({
                    "shellId": "sh_1",
                    "tool": "Bash",
                    "startedAtMs": 42
                })
            ),
            None
        );
    }

    #[test]
    fn output_poll_shows_status_then_output_lines() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "shellId": "sh_1",
                "status": "exited",
                "completed": true,
                "exitCode": 0,
                "nextCursor": 2,
                "output": "first\nsecond\n"
            }),
        )
        .unwrap();
        assert_eq!(lines, vec!["exited (code 0)", "first", "second"]);
    }

    /// The background-shell equivalent of the foreground cargo case: with
    /// a sketch the poll renders in arrival order instead of appending all
    /// of stderr after all of stdout.
    #[test]
    fn output_poll_replays_arrival_order_from_the_sketch() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "running",
                "output": "running 1 test\ntest result: ok.\n",
                "stderr": "Compiling rebon-tool\nFinished test profile\n",
                "streamOrder": "e2,o2",
                "nextCursor": 4
            }),
        )
        .unwrap();
        assert_eq!(
            lines,
            vec![
                "running",
                "Compiling rebon-tool",
                "Finished test profile",
                "running 1 test",
                "test result: ok.",
            ]
        );
    }

    /// A sketch whose counts no longer match the strings (the ring buffer
    /// evicted events, a hand-edited field) must fall back, never scramble.
    #[test]
    fn output_poll_falls_back_on_a_mismatched_sketch() {
        for sketch in ["e9,o1", "o1,o1", "garbage"] {
            let lines = lines_for(
                "ShellOutput",
                json!({
                    "status": "running",
                    "output": "progress\n",
                    "stderr": "warning: slow\n",
                    "streamOrder": sketch,
                    "nextCursor": 2
                }),
            )
            .unwrap();
            assert_eq!(
                lines,
                vec!["running", "progress", "warning: slow"],
                "sketch {sketch:?}"
            );
        }
    }

    /// The sketch must not change how presence is judged: a poll carrying
    /// only whitespace still advanced the cursor.
    #[test]
    fn output_poll_presence_is_unaffected_by_the_sketch() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "running",
                "output": " \n",
                "stderr": " \n",
                "streamOrder": "e1,o1",
                "nextCursor": 2
            }),
        )
        .unwrap();
        assert_ne!(lines[0], "running (no new output)");
    }

    #[test]
    fn output_poll_renders_stderr_after_stdout() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "running",
                "output": "progress\n",
                "stderr": "warning: slow\n",
                "nextCursor": 2
            }),
        )
        .unwrap();
        assert_eq!(lines, vec!["running", "progress", "warning: slow"]);

        // stderr-only output must not read as "no new output".
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "running",
                "stderr": "boom\n",
                "nextCursor": 1
            }),
        )
        .unwrap();
        assert_eq!(lines, vec!["running", "boom"]);
    }

    #[test]
    fn whitespace_only_output_is_not_reported_as_no_new_output() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "running",
                "output": "\n",
                "nextCursor": 2
            }),
        )
        .unwrap();
        // The cursor advanced — the status line must not claim silence,
        // and the blank text itself renders nothing.
        assert_eq!(lines, vec!["running"]);
    }

    #[test]
    fn output_poll_without_output_says_so() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "shellId": "sh_1",
                "status": "running",
                "completed": false,
                "nextCursor": 0,
                "waitTimedOut": true
            }),
        )
        .unwrap();
        assert_eq!(lines, vec!["running · no new output"]);
    }

    #[test]
    fn output_poll_appends_buffer_hints() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "running",
                "output": "tail",
                "hasMore": true,
                "cursorTruncated": true,
                "nextCursor": 9
            }),
        )
        .unwrap();
        assert_eq!(
            lines[0],
            "running · more output buffered · earlier output dropped"
        );
        assert_eq!(lines[1], "tail");
    }

    #[test]
    fn failed_shell_shows_first_error_line() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "failed",
                "exitCode": 1,
                "error": "spawn refused\nsecond line",
                "nextCursor": 0
            }),
        )
        .unwrap();
        assert_eq!(
            lines,
            vec!["failed (code 1) · spawn refused · no new output"]
        );
    }

    #[test]
    fn an_events_array_still_renders_grouped_by_stream() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "status": "exited",
                "exitCode": 0,
                "nextCursor": 2,
                "events": [
                    { "cursor": 0, "stream": "stdout", "text": "a\nb" },
                    { "cursor": 1, "stream": "stderr", "text": "!\n" }
                ]
            }),
        )
        .unwrap();
        assert_eq!(lines, vec!["exited (code 0)", "a", "b", "!"]);
    }

    #[test]
    fn list_mode_renders_one_line_per_shell() {
        let lines = lines_for(
            "ShellOutput",
            json!({
                "shells": [
                    {
                        "shellId": "sh_a",
                        "status": "running",
                        "command": "npm  run\ndev"
                    },
                    {
                        "shellId": "sh_b",
                        "status": "exited",
                        "exitCode": 1,
                        "command": "cargo test"
                    }
                ]
            }),
        )
        .unwrap();
        assert_eq!(
            lines,
            vec![
                "sh_a · running · npm run dev",
                "sh_b · exited (code 1) · cargo test",
            ]
        );
    }

    #[test]
    fn list_mode_truncates_long_commands() {
        let command = "x".repeat(LIST_COMMAND_MAX_CHARS + 10);
        let lines = lines_for(
            "ShellOutput",
            json!({
                "shells": [{ "shellId": "sh_a", "status": "running", "command": command }]
            }),
        )
        .unwrap();
        assert!(lines[0].ends_with('\u{2026}'));
        assert!(lines[0].chars().count() < LIST_COMMAND_MAX_CHARS + 30);
    }

    #[test]
    fn empty_list_mode_says_no_shells() {
        let lines = lines_for("ShellOutput", json!({ "shells": [] })).unwrap();
        assert_eq!(lines, vec!["no background shells"]);
    }

    #[test]
    fn shell_stop_renders_single_status_line() {
        let lines = lines_for(
            "ShellStop",
            json!({
                "shellId": "sh_a",
                "status": "stopping",
                "stopRequested": true,
                "alreadyRequested": false,
                "alreadyCompleted": false
            }),
        )
        .unwrap();
        assert_eq!(lines, vec!["stopping"]);

        let lines = lines_for(
            "ShellStop",
            json!({
                "shellId": "sh_a",
                "status": "exited",
                "exitCode": 0,
                "alreadyCompleted": true
            }),
        )
        .unwrap();
        assert_eq!(lines, vec!["exited (code 0) · was already finished"]);
    }

    #[test]
    fn header_summary_prefers_originating_command() {
        let raw_output = json!({
            "shellId": "sh_1",
            "command": "npm  run\ndev",
            "status": "running",
            "nextCursor": 0
        });
        assert_eq!(
            shell_management_header_summary_from_value("ShellOutput", &raw_output).as_deref(),
            Some("npm run dev")
        );
        assert_eq!(
            shell_management_header_summary_from_value("ShellStop", &raw_output).as_deref(),
            Some("npm run dev")
        );
        // Other tools and command-less results keep their own summary.
        assert_eq!(
            shell_management_header_summary_from_value("Bash", &raw_output),
            None
        );
        assert_eq!(
            shell_management_header_summary_from_value(
                "ShellOutput",
                &json!({ "shellId": "sh_1", "status": "running" })
            ),
            None
        );
        assert_eq!(
            shell_management_header_summary_from_value("ShellOutput", &json!({ "command": " " })),
            None
        );

        let map: HashMap<String, Value> = raw_output
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(
            shell_management_header_summary("ShellOutput", &map).as_deref(),
            Some("npm run dev")
        );
    }

    #[test]
    fn header_summary_truncates_long_commands() {
        let raw_output = json!({
            "command": "y".repeat(LIST_COMMAND_MAX_CHARS + 5),
            "status": "running"
        });
        let summary =
            shell_management_header_summary_from_value("ShellOutput", &raw_output).unwrap();
        assert!(summary.ends_with('\u{2026}'));
        assert_eq!(summary.chars().count(), LIST_COMMAND_MAX_CHARS + 1);
    }

    #[test]
    fn hashmap_entry_point_matches_value_entry_point() {
        let value = json!({
            "status": "timed_out",
            "output": "late\n",
            "nextCursor": 3
        });
        let map: HashMap<String, Value> = value
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(
            shell_management_body_lines("ShellOutput", &map),
            lines_for("ShellOutput", value)
        );
        assert_eq!(
            shell_management_body_lines("ShellOutput", &map).unwrap()[0],
            "timed out"
        );
    }
}
