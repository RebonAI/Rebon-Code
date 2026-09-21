//! Monotonic stable-prefix advance for streaming markdown.
//!
//! ## What the algorithm does
//!
//! A streaming assistant message is re-rendered on every delta, so the text
//! is split at its last top-level block boundary: everything before that
//! boundary is stable (memoized, never re-parsed) and only the final block is
//! re-parsed per delta. A code fence that has not been closed yet lexes as one
//! token, so the boundary can never land inside it.
//!
//! Five load-bearing invariants:
//!
//! 1. **Strip first.** `strip_prompt_xml_tags` runs **before** the
//!    boundary check so the prefix tracker sees the same string the
//!    non-streaming render path memoizes on.
//! 2. **Reset if not a prefix.** New text that does not have the
//!    cached prefix as its `starts_with` triggers a full reset. This
//!    is the "text was replaced" defence — it is also the path that
//!    handles a closing tag arriving and causing `stripped(N+1)` to
//!    be shorter than `stripped(N)`.
//! 3. **Lex from boundary, not from start.** Saves the cost of
//!    re-tokenising the stable prefix on every delta.
//! 4. **Last non-space token is the growing block.** Everything
//!    before is committable. `Space` tokens between paragraphs do
//!    not count as "content", so the boundary can advance past them.
//! 5. **Monotonic, never retreating advance.** Guarding the assignment
//!    with `advance > 0` means a delta that doesn't add new content
//!    (still inside the same growing block) leaves the prefix unchanged.
//!
//! ## Why the lexer is injected, not embedded
//!
//! [`StreamingBoundary::advance`] takes the lexer as a `FnOnce(&str) ->
//! Vec<LexedToken>` parameter, so the algorithm can be tested without
//! pulling in a Markdown parser. The shape the algorithm cares about
//! is just `(kind, raw_length)` per token, plus a way to identify the
//! `Space` variant. That's exactly what `LexedToken` exposes.
//!
//! Production passes `rebon_message_tui`'s `cmark_block_lex`, a
//! `pulldown-cmark` block lexer; the tests below use hand-crafted token
//! vectors.

use crate::strip_prompt_tags::strip_prompt_xml_tags;

/// Discriminant for the `Space` vs everything-else distinction the
/// boundary algorithm cares about. Every richer block kind a Markdown
/// lexer can emit collapses into `Other` here — only the bit the
/// algorithm branches on is modelled. A lexer that knows richer kinds
/// can map them onto this enum at the seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// A blank-line gap between blocks. The algorithm skips trailing
    /// `Space` tokens when computing the "last content" cut-off.
    Space,
    /// Anything else — paragraph, code, list, table, html, etc. The
    /// algorithm does not branch on which one; only the length of `raw`
    /// matters for the byte advance.
    Other,
}

/// One lexer output token, reduced to the two fields the boundary
/// algorithm reads. `raw` carries the exact input substring the token
/// was lexed from; the algorithm sums the `raw.len()` values of those
/// tokens to compute the byte advance for the stable prefix.
///
/// `raw` is `String` (not `&str`) so the lexer can return a fully
/// owned token vector — common Markdown parsers either return
/// borrowed slices or owned strings depending on the API, and the
/// boundary algorithm shouldn't impose a lifetime on its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedToken {
    pub kind: TokenKind,
    pub raw: String,
}

impl LexedToken {
    /// Convenience for tests and synthetic lexers — `Other` token
    /// with the given raw byte content.
    pub fn other(raw: impl Into<String>) -> Self {
        Self {
            kind: TokenKind::Other,
            raw: raw.into(),
        }
    }

    /// Convenience for tests — `Space` token with the given raw
    /// byte content.
    pub fn space(raw: impl Into<String>) -> Self {
        Self {
            kind: TokenKind::Space,
            raw: raw.into(),
        }
    }
}

/// Stateful tracker for the monotonic stable prefix of one streaming
/// assistant message.
/// One instance per streaming render-cycle of
/// one assistant message; reset on cancel/unmount by simply dropping
/// it (or by calling [`StreamingBoundary::reset`] for in-place reuse).
///
/// The state is private and the only mutator is [`Self::advance`],
/// which both modifies the prefix and returns the current
/// `(stable, unstable)` split. This pins the monotonic invariant in
/// the type system: callers can't accidentally rewind without
/// re-creating the boundary.
#[derive(Debug, Default, Clone)]
pub struct StreamingBoundary {
    stable_prefix: String,
}

/// Output of [`StreamingBoundary::advance`]: the two halves of the
/// stripped text — the memoized prefix and the in-flight suffix —
/// that the caller renders separately so only the suffix re-parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundarySplit {
    pub stable_prefix: String,
    pub unstable_suffix: String,
}

impl StreamingBoundary {
    /// New empty boundary: nothing is stable until the first
    /// [`Self::advance`] moves the boundary forward.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset to empty, as happens between turns when a fresh boundary
    /// is built. Tests use this to verify
    /// the reset branch independently.
    pub fn reset(&mut self) {
        self.stable_prefix.clear();
    }

    /// Read-only access to the current stable prefix. Callers
    /// rarely need this — `advance` returns it as part of the
    /// split — but it's useful for asserting the state in tests.
    pub fn stable_prefix(&self) -> &str {
        &self.stable_prefix
    }

    /// Process one streaming delta:
    ///
    /// 1. Strip the four reserved wrapper tags.
    /// 2. If the new stripped text doesn't start with the cached
    ///    prefix, reset the prefix to empty (a "text was
    ///    replaced" defence).
    /// 3. Lex the substring **after** the cached prefix.
    /// 4. Find the last non-space token; sum the `raw.len()` of
    ///    every earlier token; advance the cached prefix by that
    ///    many bytes.
    /// 5. Return the `(prefix, suffix)` split.
    ///
    /// `lex` is invoked at most once per call, on the substring of
    /// the stripped text that comes **after** the current boundary.
    /// The lexer must return tokens whose concatenated `raw` fields
    /// reproduce the input unchanged — every lexer at this seam is
    /// required to hold that contract.
    pub fn advance(
        &mut self,
        text: &str,
        lex: impl FnOnce(&str) -> Vec<LexedToken>,
    ) -> BoundarySplit {
        // Step 1 — strip wrapper tags so the prefix tracker sees the
        // same shape the non-streaming path memoizes.
        let stripped = strip_prompt_xml_tags(text);

        // Step 2 — defensive reset.
        if !stripped.starts_with(&self.stable_prefix) {
            self.stable_prefix.clear();
        }

        // Step 3 — lex only the unstable suffix.
        let boundary = self.stable_prefix.len();
        // SAFETY of the indexing: `stable_prefix` always equals a
        // byte-prefix of some earlier `stripped` value. Either:
        //   - The current `stripped` still has it as a prefix
        //     (verified above), so `boundary` is a valid char
        //     boundary in `stripped`.
        //   - We just cleared it to empty, so `boundary == 0` which
        //     is always a valid char boundary.
        // Either way, `&stripped[boundary..]` is sound.
        let unstable_input = &stripped[boundary..];
        let tokens = lex(unstable_input);

        // Step 4 — find last non-space token and sum prior raws.
        let mut last_content_idx = tokens.len() as isize - 1;
        while last_content_idx >= 0 && tokens[last_content_idx as usize].kind == TokenKind::Space {
            last_content_idx -= 1;
        }
        let mut advance = 0usize;
        if last_content_idx > 0 {
            for token in tokens.iter().take(last_content_idx as usize) {
                advance += token.raw.len();
            }
        }

        if advance > 0 {
            // Re-slice from the stripped buffer rather than appending
            // to the existing prefix, so the prefix is exactly the
            // first `boundary + advance` bytes of `stripped`.
            self.stable_prefix = stripped[..boundary + advance].to_string();
        }

        let stable_prefix = self.stable_prefix.clone();
        let unstable_suffix = stripped[stable_prefix.len()..].to_string();

        BoundarySplit {
            stable_prefix,
            unstable_suffix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic lexer that splits on blank lines into "Other"
    /// tokens with `Space` tokens between them. Close enough to a real
    /// paragraph-only block lexer that the boundary algorithm produces
    /// the same advances. Used by most tests below.
    fn paragraph_lexer(input: &str) -> Vec<LexedToken> {
        if input.is_empty() {
            return vec![];
        }
        let mut tokens = Vec::new();
        let mut remaining = input;
        while !remaining.is_empty() {
            // Look for the next blank line `\n\n`.
            if let Some(idx) = remaining.find("\n\n") {
                let body = &remaining[..idx];
                if !body.is_empty() {
                    tokens.push(LexedToken::other(body));
                }
                tokens.push(LexedToken::space("\n\n"));
                remaining = &remaining[idx + 2..];
            } else {
                tokens.push(LexedToken::other(remaining));
                break;
            }
        }
        tokens
    }

    // -----------------------------------------------------------------
    // Single-delta cases
    // -----------------------------------------------------------------

    #[test]
    fn empty_input_is_no_op() {
        let mut b = StreamingBoundary::new();
        let split = b.advance("", paragraph_lexer);
        assert_eq!(split.stable_prefix, "");
        assert_eq!(split.unstable_suffix, "");
    }

    #[test]
    fn single_token_yields_no_advance() {
        // One paragraph with no completed blocks → entire text is
        // unstable. The last content token is the first one, so there
        // is nothing before it to sum.
        let mut b = StreamingBoundary::new();
        let split = b.advance("hello world", paragraph_lexer);
        assert_eq!(split.stable_prefix, "");
        assert_eq!(split.unstable_suffix, "hello world");
    }

    #[test]
    fn two_paragraphs_separated_by_blank_advances_to_first() {
        // Tokens: [Other("p1"), Space("\n\n"), Other("p2")]
        // last content = idx 2 (p2). Sum of prior raws = 2 + 2 = 4
        // (p1 + \n\n). Stable prefix = "p1\n\n".
        let mut b = StreamingBoundary::new();
        let split = b.advance("p1\n\np2", paragraph_lexer);
        assert_eq!(split.stable_prefix, "p1\n\n");
        assert_eq!(split.unstable_suffix, "p2");
    }

    #[test]
    fn trailing_space_token_does_not_count_as_content() {
        // Direct test of the algorithm without strip-prompt-tags
        // interference: feed a synthetic input that survives the
        // .trim() in strip_prompt_xml_tags. The boundary should
        // recognize a trailing Space token as non-content and leave
        // the prefix unadvanced.
        //
        // Use input "p1\n\nx" and a custom lexer that returns
        // [Other("p1"), Space("\n\n"), Other("x"), Space("trailing")]
        // — the trailing Space at the end is what we're testing.
        // The backwards scan stops at Other("x") at idx 2. Sum prior
        // raws: 2 + 2 = 4, so the prefix is the first paragraph and its
        // blank line.
        let mut b = StreamingBoundary::new();
        let split = b.advance("p1\n\nx", |_| {
            vec![
                LexedToken::other("p1"),
                LexedToken::space("\n\n"),
                LexedToken::other("x"),
                LexedToken::space(""),
            ]
        });
        assert_eq!(split.stable_prefix, "p1\n\n");
        assert_eq!(split.unstable_suffix, "x");
    }

    #[test]
    fn strip_prompt_tags_trims_trailing_blank_lines_before_lex() {
        // The strip_prompt_xml_tags pass calls .trim() at the end,
        // which eats trailing whitespace including \n\n. So a
        // streaming text of "p1\n\n" arrives at the boundary
        // algorithm as "p1" — a single non-space token, no advance.
        // That trim happens before the boundary check, and the test pins
        // the outcome.
        let mut b = StreamingBoundary::new();
        let split = b.advance("p1\n\n", paragraph_lexer);
        assert_eq!(split.stable_prefix, "");
        assert_eq!(split.unstable_suffix, "p1");
    }

    // -----------------------------------------------------------------
    // Multi-delta monotonic advance — the load-bearing invariant
    // -----------------------------------------------------------------

    #[test]
    fn appending_to_growing_block_does_not_retreat() {
        let mut b = StreamingBoundary::new();
        let s1 = b.advance("p1\n\np2", paragraph_lexer);
        assert_eq!(s1.stable_prefix, "p1\n\n");

        // Stream more characters into the unfinished p2 block.
        let s2 = b.advance("p1\n\np2 plus more", paragraph_lexer);
        assert_eq!(
            s2.stable_prefix, "p1\n\n",
            "stable prefix must not retreat or grow while still in same block"
        );
        assert_eq!(s2.unstable_suffix, "p2 plus more");
    }

    #[test]
    fn appending_to_completed_block_grows_prefix() {
        let mut b = StreamingBoundary::new();
        let _ = b.advance("p1\n\np2", paragraph_lexer);
        let s2 = b.advance("p1\n\np2\n\np3", paragraph_lexer);
        // Now p2 is also complete. Tokens for the new lex from
        // boundary "p1\n\n" are: [Other("p2"), Space, Other("p3")].
        // Advance from boundary = 2 + 2 = 4 (p2 + \n\n).
        // New prefix = "p1\n\n" + "p2\n\n" = "p1\n\np2\n\n".
        assert_eq!(s2.stable_prefix, "p1\n\np2\n\n");
        assert_eq!(s2.unstable_suffix, "p3");
    }

    #[test]
    fn many_consecutive_advances_preserve_monotonicity() {
        let mut b = StreamingBoundary::new();
        let inputs = ["p1", "p1\n", "p1\n\n", "p1\n\np2", "p1\n\np2 more"];
        let mut last_prefix_len = 0usize;
        for input in inputs {
            let split = b.advance(input, paragraph_lexer);
            assert!(
                split.stable_prefix.len() >= last_prefix_len,
                "prefix retreated on input {input:?}: was {last_prefix_len}, now {}",
                split.stable_prefix.len()
            );
            last_prefix_len = split.stable_prefix.len();
        }
    }

    // -----------------------------------------------------------------
    // Reset path — "text was replaced"
    // -----------------------------------------------------------------

    #[test]
    fn replaced_text_resets_prefix() {
        let mut b = StreamingBoundary::new();
        let _ = b.advance("p1\n\np2", paragraph_lexer);
        assert_eq!(b.stable_prefix(), "p1\n\n");

        // New stream — completely unrelated text. Prefix must reset.
        let split = b.advance("totally different\n\nstart", paragraph_lexer);
        // Tokens: [Other("totally different"), Space, Other("start")]
        // → advance to "totally different\n\n".
        assert_eq!(split.stable_prefix, "totally different\n\n");
        assert_eq!(split.unstable_suffix, "start");
    }

    #[test]
    fn shrinking_text_resets_prefix() {
        let mut b = StreamingBoundary::new();
        let _ = b.advance("p1\n\np2 plus more text here", paragraph_lexer);
        assert_eq!(b.stable_prefix(), "p1\n\n");

        // Now the shrunk text drops the trailing block: the new text is
        // the paragraph alone, which the cached prefix (paragraph plus
        // its blank line) does not prefix, so the reset path runs.
        let split = b.advance("p1", paragraph_lexer);
        assert_eq!(split.stable_prefix, "");
        assert_eq!(split.unstable_suffix, "p1");
    }

    #[test]
    fn explicit_reset_clears_state() {
        let mut b = StreamingBoundary::new();
        let _ = b.advance("p1\n\np2", paragraph_lexer);
        assert!(!b.stable_prefix().is_empty());
        b.reset();
        assert_eq!(b.stable_prefix(), "");
    }

    // -----------------------------------------------------------------
    // strip_prompt_xml_tags integration
    // -----------------------------------------------------------------

    #[test]
    fn wrapper_tags_are_stripped_before_lex() {
        let mut b = StreamingBoundary::new();
        let split = b.advance("<context>noise</context>p1\n\np2", paragraph_lexer);
        // After strip + trim: "p1\n\np2"
        assert_eq!(split.stable_prefix, "p1\n\n");
        assert_eq!(split.unstable_suffix, "p2");
    }

    #[test]
    fn wrapper_close_arriving_does_not_break_monotonic() {
        // Simulate the "closing tag arrives → stripped
        // shrinks" path. The reset branch handles it, so the prefix
        // recomputes from the new (smaller) stripped string instead
        // of carrying stale bytes.
        let mut b = StreamingBoundary::new();
        // First delta — open tag, no close yet. Strip is a no-op.
        let s1 = b.advance("p1\n\n<context>noise", paragraph_lexer);
        // tokens: [Other("p1"), Space, Other("<context>noise")]
        // advance = 2 (p1) + 2 (\n\n) = 4
        // stable_prefix = "p1\n\n"
        assert_eq!(s1.stable_prefix, "p1\n\n");

        // Second delta — close tag arrives. Strip now removes the
        // wrapper, so `stripped` is just "p1" — much shorter than
        // the previous prefix "p1\n\n". The reset branch fires.
        let s2 = b.advance("p1\n\n<context>noise</context>", paragraph_lexer);
        // After strip+trim: "p1"
        // "p1" does not start with "p1\n\n" → reset.
        // Re-lex "p1": [Other("p1")]. last content idx = 0. advance = 0.
        // → prefix stays empty, unstable = "p1".
        assert_eq!(s2.stable_prefix, "");
        assert_eq!(s2.unstable_suffix, "p1");
    }

    // -----------------------------------------------------------------
    // Lexer contract — token raws must reproduce input
    // -----------------------------------------------------------------

    #[test]
    fn algorithm_uses_byte_lengths_not_char_counts() {
        // CJK characters are multi-byte. The algorithm sums the raw byte
        // length of each token, not UTF-16 code units. For pure paragraph
        // splits the boundary still falls on a UTF-8 char boundary
        // because the splitter only ever splits on the ASCII blank-line
        // separator.
        fn cjk_lexer(input: &str) -> Vec<LexedToken> {
            paragraph_lexer(input)
        }

        let mut b = StreamingBoundary::new();
        let split = b.advance("你好\n\n世界", cjk_lexer);
        // Tokens: [Other("你好"), Space("\n\n"), Other("世界")]
        // advance = 6 (你好 = 2×3 bytes) + 2 (\n\n) = 8
        assert_eq!(split.stable_prefix, "你好\n\n");
        assert_eq!(split.unstable_suffix, "世界");
    }

    // -----------------------------------------------------------------
    // Cross-delta sanity: prefix + suffix always reproduces stripped
    // -----------------------------------------------------------------

    #[test]
    fn split_concatenation_equals_stripped_input_for_each_delta() {
        let mut b = StreamingBoundary::new();
        let inputs = [
            "p1",
            "p1\n",
            "p1\n\n",
            "p1\n\np2",
            "p1\n\np2 more",
            "p1\n\np2 more\n\np3",
        ];
        for input in inputs {
            let split = b.advance(input, paragraph_lexer);
            let recombined = format!("{}{}", split.stable_prefix, split.unstable_suffix);
            let stripped = strip_prompt_xml_tags(input);
            assert_eq!(
                recombined, stripped,
                "split must reproduce stripped input for {input:?}",
            );
        }
    }
}
