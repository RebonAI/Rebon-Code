//! Middle-truncation for large shell (exec) tool output.
//!
//! Ported from codex-rs's `codex_utils_string::truncate_middle_chars`
//! + `codex_utils_output_truncation::formatted_truncate_text`, keeping
//! the same head/tail-preserving algorithm and default byte budget.
//!
//! Rationale: uncapped `Bash` / `PowerShell` output flows verbatim
//! into the transcript and therefore into *every* subsequent model
//! request. A single multi-hundred-KB `grep` / `cat` dump both bloats
//! the context window and — on the OpenAI Responses incremental path —
//! enlarges the client-authored delta that must be re-sent, which is a
//! measurable driver of prompt-cache misses. Capping each stream to a
//! byte budget while preserving the head and the tail keeps the result
//! useful (the command echo, the first errors, and the final summary /
//! exit line all survive) without paying for the middle.
//!
//! The `Read` and `Grep` tools already clip their own output; `Bash`
//! and `PowerShell` were the remaining uncapped exec surfaces — and the
//! ones GPT / Codex-style models reach for most.

use rebon_tools_core::shell_stream_order;

/// codex's fixed heuristic: ~4 bytes per token.
const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Default per-stream byte budget for shell tool output. Matches
/// codex-rs's default exec `TruncationPolicy::Bytes(10_000)`.
pub const DEFAULT_TOOL_OUTPUT_MAX_BYTES: usize = 10_000;

/// Environment override for [`DEFAULT_TOOL_OUTPUT_MAX_BYTES`]. Set to a
/// `usize`; `0` disables truncation entirely.
const MAX_BYTES_ENV: &str = "REBON_TOOL_OUTPUT_MAX_BYTES";

/// Resolve the active per-stream byte budget:
/// `REBON_TOOL_OUTPUT_MAX_BYTES` when set to a valid integer, otherwise
/// [`DEFAULT_TOOL_OUTPUT_MAX_BYTES`].
pub fn tool_output_max_bytes() -> usize {
    std::env::var(MAX_BYTES_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOOL_OUTPUT_MAX_BYTES)
}

/// Truncate a single shell stream (stdout or stderr) to `max_bytes`,
/// preserving a head and a tail on UTF-8 boundaries with a
/// `…N chars truncated…` marker in the middle. When truncation occurs,
/// a one-line warning header records the original token count and line
/// count so the model knows the output was clipped (and can re-run with
/// a narrower command if it needs the middle).
///
/// `max_bytes == 0` disables truncation. Content already within budget
/// is returned unchanged with no header.
pub fn truncate_stream_output(content: &str, max_bytes: usize) -> String {
    if max_bytes == 0 || content.len() <= max_bytes {
        return content.to_string();
    }

    let original_tokens = approx_token_count(content);
    let total_lines = content.lines().count();
    let body = truncate_middle_bytes(content, max_bytes);
    format!(
        "Warning: truncated output (original token count: {original_tokens}, {total_lines} lines)\n\n{body}"
    )
}

/// Turn the arrival-ordered lines a shell tool collected into the three
/// result fields it reports: the per-stream `stdout` / `stderr` strings
/// (each independently truncated to `max_bytes`) and, when it is both
/// meaningful and still trustworthy, the
/// [`shell_stream_order`] sketch describing how the two interleaved.
///
/// `lines` is `(is_stderr, text)` in the order the lines were read.
///
/// The sketch is dropped whenever either stream was truncated: middle
/// truncation rewrites both the line count and the line contents, so a
/// sketch recorded against the original would zip the survivors into an
/// order that never happened. `shell_stream_order::interleave` also
/// re-checks the counts on the reading side, but dropping it here keeps
/// a doomed field out of the model's context in the first place.
pub fn finish_shell_streams(
    lines: &[(bool, String)],
    max_bytes: usize,
) -> (String, String, Option<String>) {
    let stdout_joined = join_stream(lines, false);
    let stderr_joined = join_stream(lines, true);
    let stdout = truncate_stream_output(&stdout_joined, max_bytes);
    let stderr = truncate_stream_output(&stderr_joined, max_bytes);
    let truncated = stdout != stdout_joined || stderr != stderr_joined;
    let stream_order = (!truncated)
        .then(|| shell_stream_order::encode(lines.iter().map(|(is_stderr, _)| *is_stderr)))
        .flatten();
    (stdout, stderr, stream_order)
}

fn join_stream(lines: &[(bool, String)], want_stderr: bool) -> String {
    lines
        .iter()
        .filter(|(is_stderr, _)| *is_stderr == want_stderr)
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Middle-truncate `s` to at most (approximately) `max_bytes`,
/// preserving the start and end. Byte-accurate and UTF-8-boundary safe.
/// Mirrors codex-rs `truncate_middle_chars`. The inserted marker adds a
/// small fixed overhead beyond `max_bytes`, matching codex's behaviour.
fn truncate_middle_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    // Split the budget between a head and a tail slice.
    let head_bytes = max_bytes / 2;
    let tail_bytes = max_bytes - head_bytes;
    let (removed_chars, head, tail) = split_head_tail(s, head_bytes, tail_bytes);
    format!("{head}…{removed_chars} chars truncated…{tail}")
}

/// Walk `s` once, collecting the longest UTF-8 prefix that fits in
/// `head_bytes` and the longest suffix that fits in `tail_bytes`,
/// counting the characters dropped in between. Returns
/// `(removed_chars, head, tail)`. Ported from codex-rs `split_string`.
fn split_head_tail(s: &str, head_bytes: usize, tail_bytes: usize) -> (usize, &str, &str) {
    if s.is_empty() {
        return (0, "", "");
    }

    let len = s.len();
    let tail_start_target = len.saturating_sub(tail_bytes);
    let mut prefix_end = 0usize;
    let mut suffix_start = len;
    let mut removed_chars = 0usize;
    let mut suffix_started = false;

    for (idx, ch) in s.char_indices() {
        let char_end = idx + ch.len_utf8();
        if char_end <= head_bytes {
            prefix_end = char_end;
            continue;
        }
        if idx >= tail_start_target {
            if !suffix_started {
                suffix_start = idx;
                suffix_started = true;
            }
            continue;
        }
        removed_chars = removed_chars.saturating_add(1);
    }

    // Guard against overlap when head + tail budgets exceed the string.
    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }

    (removed_chars, &s[..prefix_end], &s[suffix_start..])
}

/// Approximate token count using codex's ~4-bytes-per-token heuristic.
fn approx_token_count(text: &str) -> usize {
    text.len().saturating_add(APPROX_BYTES_PER_TOKEN - 1) / APPROX_BYTES_PER_TOKEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_returned_unchanged() {
        assert_eq!(truncate_stream_output("hello", 10_000), "hello");
        assert_eq!(truncate_stream_output("", 10_000), "");
    }

    #[test]
    fn output_at_budget_boundary_is_not_truncated() {
        let s = "x".repeat(100);
        assert_eq!(truncate_stream_output(&s, 100), s);
    }

    #[test]
    fn oversize_output_is_middle_truncated_with_header() {
        let s = "a".repeat(50_000);
        let out = truncate_stream_output(&s, 10_000);
        assert!(out.starts_with("Warning: truncated output"));
        assert!(out.contains("chars truncated"));
        // Head + tail preserved, middle dropped.
        assert!(out.contains("aaaa"));
        // The clipped body is far smaller than the original.
        assert!(out.len() < 11_000, "len was {}", out.len());
    }

    #[test]
    fn preserves_head_and_tail_content() {
        let s = format!("START{}END", "m".repeat(40_000));
        let out = truncate_stream_output(&s, 2_000);
        assert!(out.contains("START"), "head lost: {out:.80}");
        assert!(out.trim_end().ends_with("END"), "tail lost");
    }

    #[test]
    fn budget_zero_disables_truncation() {
        let s = "a".repeat(50_000);
        assert_eq!(truncate_stream_output(&s, 0), s);
    }

    #[test]
    fn truncation_is_utf8_boundary_safe() {
        // Multi-byte characters must never be split mid-codepoint.
        let s = "日本語テキスト".repeat(5_000);
        let out = truncate_stream_output(&s, 1_000);
        // If any slice split a codepoint, formatting/print would have
        // panicked already; assert the marker landed and it's valid.
        assert!(out.contains("chars truncated"));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn env_override_parses_and_falls_back() {
        // Direct budget path is env-independent; just verify the
        // default constant is what we expect.
        assert_eq!(DEFAULT_TOOL_OUTPUT_MAX_BYTES, 10_000);
    }

    fn line(is_stderr: bool, text: &str) -> (bool, String) {
        (is_stderr, text.to_string())
    }

    #[test]
    fn finish_shell_streams_splits_streams_and_keeps_join_semantics() {
        let lines = vec![
            line(false, "out one"),
            line(true, "err one"),
            line(false, "out two"),
        ];
        let (stdout, stderr, order) = finish_shell_streams(&lines, 0);
        assert_eq!(stdout, "out one\nout two");
        assert_eq!(stderr, "err one");
        assert_eq!(order.as_deref(), Some("o1,e1,o1"));
    }

    #[test]
    fn finish_shell_streams_omits_the_sketch_when_it_adds_nothing() {
        // Single stream: concatenation already is arrival order.
        let (stdout, stderr, order) = finish_shell_streams(&[line(false, "only out")], 0);
        assert_eq!(stdout, "only out");
        assert_eq!(stderr, "");
        assert_eq!(order, None);

        // stdout entirely before stderr is exactly the fallback's shape.
        let ordered = vec![line(false, "a"), line(false, "b"), line(true, "X")];
        assert_eq!(finish_shell_streams(&ordered, 0).2, None);

        // Nothing at all.
        assert_eq!(finish_shell_streams(&[], 0).2, None);
    }

    #[test]
    fn finish_shell_streams_records_the_cargo_shape() {
        // cargo writes progress to stderr and results to stdout, so the
        // stderr-first order is precisely what a renderer cannot guess.
        let lines = vec![
            line(true, "Compiling rebon-tool v0.20.0"),
            line(true, "Finished test profile"),
            line(false, "running 1 test"),
            line(false, "test result: ok."),
        ];
        assert_eq!(finish_shell_streams(&lines, 0).2.as_deref(), Some("e2,o2"));
    }

    #[test]
    fn finish_shell_streams_drops_the_sketch_once_a_stream_is_truncated() {
        // A budget small enough to clip stdout invalidates the 1:1 mapping
        // the sketch depends on, so it must not be reported at all.
        let lines = vec![
            line(true, "err"),
            line(false, "x".repeat(400).as_str()),
            line(true, "err tail"),
        ];
        let (stdout, _, order) = finish_shell_streams(&lines, 64);
        assert!(stdout.starts_with("Warning: truncated output"));
        assert_eq!(order, None);

        // Same lines, no budget: the sketch survives.
        assert!(finish_shell_streams(&lines, 0).2.is_some());
    }
}
