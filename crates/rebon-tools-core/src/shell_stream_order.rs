//! Arrival-order metadata for interleaved shell stdout/stderr output.
//!
//! The shell tools hand the model two separate strings — `stdout` and
//! `stderr`. That shape is what the model consumes and it stays exactly as it
//! was. But splitting the streams throws away the order the lines actually
//! arrived in, and a renderer rebuilding the tool card from the finished result
//! has nothing left to go on: it concatenates stdout first and stderr after.
//! For `cargo` that guess is actively wrong — cargo writes every progress line
//! (`Compiling`, `Finished`, `Running`) to stderr and only test results to
//! stdout, so the concatenation puts the compile banner *after* the test
//! summary.
//!
//! So the tools additionally record a compact run-length sketch of the
//! interleaving: `e3,o5,e1` means "3 stderr lines, then 5 stdout lines,
//! then 1 stderr line". A renderer that finds the field zips the two
//! streams back into arrival order; one that doesn't falls back to the
//! old concatenation, so sessions recorded before this field existed
//! keep rendering exactly as they always did.
//!
//! Two deliberate limits keep the field honest and nearly free:
//!
//! - [`encode`] emits nothing when the sketch would carry no
//!   information (a single stream, or stdout entirely before stderr —
//!   which is precisely what the fallback already produces). The common
//!   single-stream command therefore costs zero extra bytes.
//! - The sketch describes the *untruncated* streams. Callers must skip
//!   it once either stream has been clipped, and [`interleave`]
//!   independently refuses any sketch whose line counts don't match what
//!   it was handed. A scrambled body is worse than a concatenated one, so
//!   every mismatch falls back instead of guessing.

/// Result-object key carrying the sketch produced by [`encode`].
pub const STREAM_ORDER_KEY: &str = "streamOrder";

const STDOUT_TAG: char = 'o';
const STDERR_TAG: char = 'e';

/// Run-length encode the stream each line arrived on (`true` = stderr),
/// in arrival order.
///
/// Returns `None` when the resulting sketch would tell a reader nothing
/// it couldn't already assume — see the module docs.
pub fn encode(streams: impl IntoIterator<Item = bool>) -> Option<String> {
    let mut runs: Vec<(bool, usize)> = Vec::new();
    for is_stderr in streams {
        match runs.last_mut() {
            Some((tag, count)) if *tag == is_stderr => *count += 1,
            _ => runs.push((is_stderr, 1)),
        }
    }
    if !carries_information(&runs) {
        return None;
    }
    let mut sketch = String::new();
    for (index, (is_stderr, count)) in runs.iter().enumerate() {
        if index > 0 {
            sketch.push(',');
        }
        sketch.push(if *is_stderr { STDERR_TAG } else { STDOUT_TAG });
        sketch.push_str(&count.to_string());
    }
    Some(sketch)
}

fn carries_information(runs: &[(bool, usize)]) -> bool {
    match runs {
        // Nothing at all, or a single stream: concatenation IS arrival order.
        [] | [_] => false,
        // stdout entirely before stderr is exactly what the fallback
        // produces, so recording it would cost bytes and change nothing.
        [(false, _), (true, _)] => false,
        _ => true,
    }
}

/// Parse a sketch into its `(is_stderr, count)` runs.
///
/// Rejects anything [`encode`] could not have produced — an unknown tag,
/// a zero-length or unparseable count, or two adjacent runs on the same
/// stream (which `encode` always merges). A corrupted field must fail
/// loudly here so the caller falls back rather than rebuilding a
/// plausible-looking wrong order.
pub fn decode(sketch: &str) -> Option<Vec<(bool, usize)>> {
    let mut runs: Vec<(bool, usize)> = Vec::new();
    for token in sketch.split(',') {
        let mut chars = token.chars();
        let is_stderr = match chars.next()? {
            STDOUT_TAG => false,
            STDERR_TAG => true,
            _ => return None,
        };
        let count: usize = chars.as_str().parse().ok()?;
        if count == 0 {
            return None;
        }
        if runs.last().is_some_and(|(tag, _)| *tag == is_stderr) {
            return None;
        }
        runs.push((is_stderr, count));
    }
    (!runs.is_empty()).then_some(runs)
}

/// Split a stream's text into the lines a sketch counts.
///
/// This is THE line-splitting rule for both sides of the contract: the
/// tool counts lines with it when building a sketch, and the renderer
/// splits with it when replaying one. They must never diverge — two
/// nearly-identical rules would put the counts out of step and silently
/// send every sketch down the fallback path.
///
/// Split on `\n`, dropping the one trailing empty segment that text
/// ending in `\n` produces. Interior blank lines are kept: they are real
/// output. An empty stream is zero lines, not one blank one.
pub fn stream_display_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Count the lines a completed run of chunk-wise output contributes to
/// each stream, in the order the lines *finished*.
///
/// Background shells read raw byte chunks, not lines, so a chunk can end
/// mid-line and a line can span several chunks. Feed the chunks here in
/// arrival order as `(is_stderr, text)` and it returns one `bool` per
/// finished line, tagged with the stream it belongs to — exactly the
/// input [`encode`] wants.
///
/// Each stream is buffered independently, so the per-stream line
/// sequence matches what [`stream_display_lines`] produces over that
/// stream's concatenated text. A line is ordered by where it *ends*,
/// which for interleaved output is the point at which it became visible.
pub fn line_streams_from_chunks<'a>(
    chunks: impl IntoIterator<Item = (bool, &'a str)>,
) -> Vec<bool> {
    let mut streams = Vec::new();
    // Whether each stream has an unterminated line in flight.
    let mut pending = [false; 2];
    for (is_stderr, text) in chunks {
        let slot = usize::from(is_stderr);
        for (index, segment) in text.split('\n').enumerate() {
            if index > 0 {
                // The '\n' that opened this segment closed the previous
                // line on this stream.
                streams.push(is_stderr);
                pending[slot] = false;
            }
            if !segment.is_empty() {
                pending[slot] = true;
            }
        }
    }
    // Trailing text with no final newline is still a line.
    for (slot, has_pending) in pending.iter().enumerate() {
        if *has_pending {
            streams.push(slot == 1);
        }
    }
    streams
}

/// Zip `stdout_lines` and `stderr_lines` back into arrival order.
///
/// Returns `None` — meaning "fall back to concatenation" — when the
/// sketch is unparseable, or when it does not account for exactly the
/// lines it was handed. The latter is the load-bearing guard: a
/// middle-truncated stream, or a caller that pre-trimmed blank lines,
/// silently changes the line counts, and an off-by-N zip would scatter
/// output across the card in an order that never happened.
pub fn interleave<'a>(
    sketch: &str,
    stdout_lines: &[&'a str],
    stderr_lines: &[&'a str],
) -> Option<Vec<&'a str>> {
    let runs = decode(sketch)?;
    let mut expected_stdout = 0usize;
    let mut expected_stderr = 0usize;
    for (is_stderr, count) in &runs {
        let slot = if *is_stderr {
            &mut expected_stderr
        } else {
            &mut expected_stdout
        };
        *slot = slot.checked_add(*count)?;
    }
    if expected_stdout != stdout_lines.len() || expected_stderr != stderr_lines.len() {
        return None;
    }

    let mut merged = Vec::with_capacity(expected_stdout + expected_stderr);
    let mut stdout_at = 0usize;
    let mut stderr_at = 0usize;
    for (is_stderr, count) in runs {
        let (source, cursor) = if is_stderr {
            (stderr_lines, &mut stderr_at)
        } else {
            (stdout_lines, &mut stdout_at)
        };
        merged.extend_from_slice(&source[*cursor..*cursor + count]);
        *cursor += count;
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, interleave, line_streams_from_chunks, stream_display_lines};

    const OUT: bool = false;
    const ERR: bool = true;

    #[test]
    fn encode_skips_sketches_that_carry_no_information() {
        // Empty, single-stream, and "stdout then stderr" all reproduce
        // themselves under the fallback concatenation.
        assert_eq!(encode([]), None);
        assert_eq!(encode([OUT]), None);
        assert_eq!(encode([ERR]), None);
        assert_eq!(encode([OUT, OUT, OUT]), None);
        assert_eq!(encode([ERR, ERR]), None);
        assert_eq!(encode([OUT, OUT, ERR, ERR]), None);
    }

    #[test]
    fn encode_records_orders_the_fallback_would_get_wrong() {
        // stderr first is the cargo case: the fallback would move the
        // compile banner behind the test summary.
        assert_eq!(encode([ERR, OUT]).as_deref(), Some("e1,o1"));
        assert_eq!(encode([ERR, ERR, OUT, OUT, OUT]).as_deref(), Some("e2,o3"));
        // Genuine interleaving, in both starting directions.
        assert_eq!(encode([OUT, ERR, OUT]).as_deref(), Some("o1,e1,o1"));
        assert_eq!(
            encode([ERR, OUT, OUT, ERR, OUT]).as_deref(),
            Some("e1,o2,e1,o1")
        );
    }

    #[test]
    fn decode_round_trips_encode() {
        for streams in [
            vec![ERR, OUT],
            vec![OUT, ERR, OUT],
            vec![ERR, ERR, OUT, OUT, OUT, ERR],
            vec![OUT, ERR, OUT, ERR, OUT, ERR],
        ] {
            let sketch = encode(streams.iter().copied()).expect("sketch carries information");
            let runs = decode(&sketch).expect("encode output decodes");
            let flattened: Vec<bool> = runs
                .iter()
                .flat_map(|(is_stderr, count)| std::iter::repeat(*is_stderr).take(*count))
                .collect();
            assert_eq!(flattened, streams, "sketch {sketch}");
        }
    }

    #[test]
    fn decode_rejects_everything_encode_could_not_have_written() {
        for bad in [
            "",       // empty field
            "o",      // tag with no count
            "x2",     // unknown stream tag
            "o0",     // zero-length run
            "o2,",    // trailing separator leaves an empty token
            "o2,,e1", // empty token in the middle
            "oh",     // unparseable count
            "o-1",    // negative count
            "o1,o2",  // adjacent same-stream runs; encode always merges
            "e1,e1",  // ditto, stderr side
            "o1;e1",  // wrong separator
        ] {
            assert_eq!(decode(bad), None, "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn interleave_restores_cargo_arrival_order() {
        // cargo writes progress to stderr and results to stdout; the
        // fallback concatenation would render Compiling/Finished last.
        let stdout = ["running 1 test", "test foo ... ok", "test result: ok."];
        let stderr = [
            "Compiling rebon-tool v0.20.0",
            "Finished test profile",
            "Running unittests",
        ];
        let sketch = encode([ERR, ERR, ERR, OUT, OUT, OUT]).expect("interleaved");
        assert_eq!(
            interleave(&sketch, &stdout, &stderr).expect("counts match"),
            vec![
                "Compiling rebon-tool v0.20.0",
                "Finished test profile",
                "Running unittests",
                "running 1 test",
                "test foo ... ok",
                "test result: ok.",
            ]
        );
    }

    #[test]
    fn interleave_handles_alternating_runs() {
        let stdout = ["a", "b", "c"];
        let stderr = ["X", "Y"];
        let sketch = encode([OUT, ERR, OUT, ERR, OUT]).expect("interleaved");
        assert_eq!(
            interleave(&sketch, &stdout, &stderr).expect("counts match"),
            vec!["a", "X", "b", "Y", "c"]
        );
    }

    #[test]
    fn interleave_falls_back_when_line_counts_disagree() {
        let sketch = encode([ERR, OUT, OUT]).expect("interleaved");
        // Truncation dropped a stdout line.
        assert_eq!(interleave(&sketch, &["a"], &["X"]), None);
        // A caller trimmed the stderr side away entirely.
        assert_eq!(interleave(&sketch, &["a", "b"], &[]), None);
        // Extra lines the sketch never accounted for.
        assert_eq!(interleave(&sketch, &["a", "b", "c"], &["X"]), None);
        // Sanity: the matching shape does succeed.
        assert!(interleave(&sketch, &["a", "b"], &["X"]).is_some());
    }

    #[test]
    fn stream_display_lines_drops_only_the_trailing_newline_segment() {
        assert_eq!(stream_display_lines(""), Vec::<&str>::new());
        assert_eq!(stream_display_lines("a"), vec!["a"]);
        assert_eq!(stream_display_lines("a\n"), vec!["a"]);
        assert_eq!(stream_display_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(stream_display_lines("a\nb\n"), vec!["a", "b"]);
        // Interior blanks are real output and must survive.
        assert_eq!(stream_display_lines("a\n\nb\n"), vec!["a", "", "b"]);
        // A lone newline is one (blank) line, not zero and not two.
        assert_eq!(stream_display_lines("\n"), vec![""]);
        assert_eq!(stream_display_lines("a\n\n"), vec!["a", ""]);
    }

    /// The counts `line_streams_from_chunks` produces must match, per
    /// stream, what `stream_display_lines` produces over that stream's
    /// concatenated text — otherwise every sketch fails `interleave`'s
    /// count check and silently falls back.
    #[test]
    fn line_streams_from_chunks_agrees_with_stream_display_lines() {
        let cases: Vec<Vec<(bool, &str)>> = vec![
            vec![(OUT, "a\n")],
            vec![(OUT, "a")],
            vec![(OUT, "a\nb")],
            vec![(OUT, "a\n\n")],
            vec![(OUT, "\n")],
            // A line split across chunks.
            vec![(OUT, "ab"), (OUT, "cd\n")],
            // A chunk that starts with the newline closing the previous line.
            vec![(OUT, "a"), (OUT, "\nb\n")],
            // Interleaved streams.
            vec![(ERR, "err1\n"), (OUT, "out1\n")],
            vec![(OUT, "o1\n"), (ERR, "e1\n"), (OUT, "o2\n")],
            // Interleaved AND chunk-split on both sides.
            vec![
                (ERR, "Comp"),
                (OUT, "run"),
                (ERR, "iling\n"),
                (OUT, "ning\n"),
            ],
        ];
        for chunks in cases {
            let streams = line_streams_from_chunks(chunks.iter().copied());
            let joined = |want_stderr: bool| {
                chunks
                    .iter()
                    .filter(|(is_stderr, _)| *is_stderr == want_stderr)
                    .map(|(_, text)| *text)
                    .collect::<String>()
            };
            let stdout_lines = streams.iter().filter(|is_stderr| !**is_stderr).count();
            let stderr_lines = streams.iter().filter(|is_stderr| **is_stderr).count();
            assert_eq!(
                stdout_lines,
                stream_display_lines(&joined(false)).len(),
                "stdout count for {chunks:?}"
            );
            assert_eq!(
                stderr_lines,
                stream_display_lines(&joined(true)).len(),
                "stderr count for {chunks:?}"
            );
        }
    }

    #[test]
    fn line_streams_from_chunks_orders_lines_by_where_they_end() {
        // A stderr line finished before the stdout line that started earlier.
        assert_eq!(
            line_streams_from_chunks([(OUT, "partial"), (ERR, "done\n"), (OUT, " rest\n")]),
            vec![ERR, OUT]
        );
        // The cargo shape, arriving chunk-wise.
        assert_eq!(
            line_streams_from_chunks(
                [(ERR, "Compiling x\nFinished\n"), (OUT, "running 1 test\n"),]
            ),
            vec![ERR, ERR, OUT]
        );
    }

    #[test]
    fn interleave_rejects_a_corrupted_sketch() {
        assert_eq!(interleave("o1,o1", &["a", "b"], &[]), None);
        assert_eq!(interleave("nonsense", &["a"], &["X"]), None);
        assert_eq!(interleave("", &["a"], &["X"]), None);
    }
}
