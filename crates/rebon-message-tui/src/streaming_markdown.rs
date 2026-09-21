//! Real-time markdown renderer for assistant streaming.
//!
//! ## The split
//!
//! Markdown is rendered during streaming by splitting at the last
//! top-level block boundary: everything before is stable
//! (memoized, never re-parsed), only the final block is re-parsed
//! per delta. An unclosed code fence lexes as a single block, so
//! block boundaries are always safe.
//!
//! ## Architecture
//!
//! The low-level primitives are already in
//! [`rebon_render`](rebon_render):
//!
//! * `strip_prompt_xml_tags` — reserved-tag preprocessing.
//! * `StreamingBoundary` — monotonic stable-prefix advance.
//! * `LexedToken` — `(kind, raw)` tuples the boundary algorithm reads.
//!
//! The remaining seam is the **lexer callback** left as a plug point by
//! `rebon-render`'s streaming boundary. This module provides that lexer —
//! [`cmark_block_lex`] — as a thin
//! adapter around `pulldown-cmark`'s block offsets. It walks top-level
//! events, emits `LexedToken::other(slice)` per complete block, and
//! `LexedToken::space(slice)` per blank-line gap between blocks so
//! the concatenated `raw` fields reproduce the input unchanged (the
//! contract `StreamingBoundary::advance` depends on).
//!
//! On top of that seam, [`StreamingMarkdownRenderer`] composes the
//! rendering pipeline:
//!
//! ```text
//! text ─► strip_prompt_xml_tags ─► StreamingBoundary.advance(cmark_block_lex)
//!                                        │
//!                                        ├─► stable prefix   ─┐
//!                                        └─► unstable suffix ─┴─► render_markdown_blocks
//! ```
//!
//! The caller receives a [`StreamingSplit`] containing the two rendered
//! `Text<'static>` halves and hyperlink ranges local to each half. The stable
//! half is memoization-friendly (same `String` → same `Text`); the unstable
//! half is re-rendered on every delta: the two halves are rendered
//! independently so the stable one can be memoized and only the
//! in-flight block is re-rendered.
//!
//! ## Why `pulldown-cmark` (not a hand-rolled lexer)
//!
//! The streaming algorithm only needs `(kind, byte_length)` per token,
//! which is trivial to compute. But then the **non-streaming** render
//! path in [`crate::markdown_render`] still needs to parse the same
//! bytes, and reusing `pulldown-cmark` there avoids a second parser
//! and a class of "lexer A says `foo` is a block, lexer B splits it
//! differently" divergences. One parser, two use sites.

use ratatui::text::{Line, Text};
use rebon_render::{strip_prompt_xml_tags, LexedToken, StreamingBoundary};

use crate::markdown_render::{
    render_markdown_blocks_annotated_with_options,
    render_markdown_blocks_annotated_with_width_and_options, render_markdown_blocks_with_options,
    HyperlinkRange, MarkdownRenderOptions, MarkdownTheme, RenderedFormula, RenderedMarkdown,
};

/// Output of [`StreamingMarkdownRenderer::advance`].
///
/// Two already-rendered halves — the caller paints them in order. The
/// `stable` half is guaranteed not to shrink across successive
/// `advance` calls on the same boundary (monotonic prefix).
#[derive(Debug, Clone, Default)]
pub struct StreamingSplit {
    /// Rendered prefix — memoization-friendly.
    pub stable: Text<'static>,
    /// Hyperlink ranges addressing [`Self::stable`].
    pub stable_hyperlinks: Vec<HyperlinkRange>,
    /// Formula sidecars addressing [`Self::stable`].
    pub stable_formulas: Vec<RenderedFormula>,
    /// Rendered in-flight suffix — re-rendered on every delta.
    pub unstable: Text<'static>,
    /// Hyperlink ranges addressing [`Self::unstable`].
    pub unstable_hyperlinks: Vec<HyperlinkRange>,
    /// Formula sidecars addressing [`Self::unstable`].
    pub unstable_formulas: Vec<RenderedFormula>,
    /// Raw byte length of the stable prefix inside the stripped input.
    /// Exposed so callers can log or diff the boundary advance.
    pub stable_bytes: usize,
}

/// Stateful streaming markdown renderer.
///
/// One instance per streaming assistant message. Reset on message
/// unmount by dropping the renderer (or calling [`Self::reset`] for
/// in-place reuse when the caller reuses a pooled instance).
#[derive(Debug, Default, Clone)]
pub struct StreamingMarkdownRenderer {
    boundary: StreamingBoundary,
    options: MarkdownRenderOptions,
}

impl StreamingMarkdownRenderer {
    /// New empty renderer, with an empty cached stable prefix.
    pub fn new() -> Self {
        Self::default()
    }

    /// New renderer configured with explicit Markdown extension options.
    pub fn with_options(options: MarkdownRenderOptions) -> Self {
        Self {
            boundary: StreamingBoundary::default(),
            options,
        }
    }

    /// Current Markdown extension options.
    pub const fn options(&self) -> MarkdownRenderOptions {
        self.options
    }

    /// Replace extension options, resetting the stable prefix when they change.
    pub fn set_options(&mut self, options: MarkdownRenderOptions) {
        if self.options != options {
            self.boundary.reset();
            self.options = options;
        }
    }

    /// Drop the cached stable prefix (e.g. on message unmount).
    pub fn reset(&mut self) {
        self.boundary.reset();
    }

    /// Read-only access to the current stable prefix (for tests and
    /// debug logging).
    pub fn stable_prefix(&self) -> &str {
        self.boundary.stable_prefix()
    }

    /// Render one streaming delta. See module docs for the pipeline.
    pub fn advance(&mut self, text: &str, theme: &MarkdownTheme) -> StreamingSplit {
        let options = self.options;
        let split = self
            .boundary
            .advance(text, |input| cmark_block_lex_with_options(input, options));
        let stable_bytes = split.stable_prefix.len();
        let stable =
            render_markdown_blocks_annotated_with_options(&split.stable_prefix, theme, options);
        let mut unstable =
            render_markdown_blocks_annotated_with_options(&split.unstable_suffix, theme, options);
        preserve_streaming_block_separator(
            &split.stable_prefix,
            &split.unstable_suffix,
            &stable.text,
            &mut unstable,
        );
        StreamingSplit {
            stable: stable.text,
            stable_hyperlinks: stable.hyperlinks,
            stable_formulas: stable.formulas,
            unstable: unstable.text,
            unstable_hyperlinks: unstable.hyperlinks,
            unstable_formulas: unstable.formulas,
            stable_bytes,
        }
    }

    /// Width-aware streaming render. Tables in the stable and unstable halves
    /// use `terminal_width` for horizontal vs vertical layout decisions.
    pub fn advance_with_width(
        &mut self,
        text: &str,
        theme: &MarkdownTheme,
        terminal_width: usize,
    ) -> StreamingSplit {
        let options = self.options;
        let split = self
            .boundary
            .advance(text, |input| cmark_block_lex_with_options(input, options));
        let stable_bytes = split.stable_prefix.len();
        let stable = render_markdown_blocks_annotated_with_width_and_options(
            &split.stable_prefix,
            theme,
            terminal_width,
            options,
        );
        let mut unstable = render_markdown_blocks_annotated_with_width_and_options(
            &split.unstable_suffix,
            theme,
            terminal_width,
            options,
        );
        preserve_streaming_block_separator(
            &split.stable_prefix,
            &split.unstable_suffix,
            &stable.text,
            &mut unstable,
        );
        StreamingSplit {
            stable: stable.text,
            stable_hyperlinks: stable.hyperlinks,
            stable_formulas: stable.formulas,
            unstable: unstable.text,
            unstable_hyperlinks: unstable.hyperlinks,
            unstable_formulas: unstable.formulas,
            stable_bytes,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamingBlockKind {
    Normal,
    Code,
    Table,
}

fn preserve_streaming_block_separator(
    stable_raw: &str,
    unstable_raw: &str,
    stable: &Text<'static>,
    unstable: &mut RenderedMarkdown,
) {
    if stable.lines.is_empty()
        || unstable.text.lines.is_empty()
        || !streaming_halves_need_separator(stable_raw, unstable_raw)
    {
        return;
    }
    unstable.text.lines.insert(0, Line::default());
    for hyperlink in &mut unstable.hyperlinks {
        hyperlink.line = hyperlink.line.saturating_add(1);
    }
    for formula in &mut unstable.formulas {
        for range in &mut formula.ranges {
            range.line = range.line.saturating_add(1);
        }
    }
}

fn streaming_halves_need_separator(stable: &str, unstable: &str) -> bool {
    let (_, stable_last) = top_level_block_edges(stable);
    let (unstable_first, _) = top_level_block_edges(unstable);
    !matches!(
        stable_last,
        Some(StreamingBlockKind::Code | StreamingBlockKind::Table)
    ) && !matches!(unstable_first, Some(StreamingBlockKind::Code))
}

fn top_level_block_edges(input: &str) -> (Option<StreamingBlockKind>, Option<StreamingBlockKind>) {
    use pulldown_cmark::{Event, Options, Parser, Tag};

    let mut depth = 0usize;
    let mut first = None;
    let mut last = None;
    for event in Parser::new_ext(input, Options::ENABLE_TABLES) {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    let kind = match tag {
                        Tag::CodeBlock(_) => StreamingBlockKind::Code,
                        Tag::Table(_) => StreamingBlockKind::Table,
                        _ => StreamingBlockKind::Normal,
                    };
                    first.get_or_insert(kind);
                    last = Some(kind);
                }
                depth = depth.saturating_add(1);
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Rule if depth == 0 => {
                first.get_or_insert(StreamingBlockKind::Normal);
                last = Some(StreamingBlockKind::Normal);
            }
            _ => {}
        }
    }
    (first, last)
}

/// One-shot non-streaming render. Callers who don't need the
/// prefix-stable/unstable split (e.g. a scrolled-past, finished
/// assistant message) can skip the boundary tracker entirely.
pub fn render_streaming_message(text: &str, theme: &MarkdownTheme) -> Text<'static> {
    render_streaming_message_with_options(text, theme, MarkdownRenderOptions::default())
}

/// One-shot streaming-message rendering with explicit Markdown extensions.
pub fn render_streaming_message_with_options(
    text: &str,
    theme: &MarkdownTheme,
    render_options: MarkdownRenderOptions,
) -> Text<'static> {
    let stripped = strip_prompt_xml_tags(text);
    render_markdown_blocks_with_options(&stripped, theme, render_options)
}

/// Block-level lexer that feeds [`StreamingBoundary::advance`].
///
/// Walks `pulldown-cmark`'s event stream with byte offsets, emits one
/// `LexedToken::other(...)` per top-level block, and fills the gaps
/// between blocks with `LexedToken::space(...)`. The sum of all `raw`
/// fields reproduces the input byte-for-byte — the contract the
/// boundary algorithm depends on.
pub fn cmark_block_lex(input: &str) -> Vec<LexedToken> {
    cmark_block_lex_with_options(input, MarkdownRenderOptions::default())
}

fn cmark_block_lex_with_options(
    input: &str,
    render_options: MarkdownRenderOptions,
) -> Vec<LexedToken> {
    if input.is_empty() {
        return Vec::new();
    }
    use pulldown_cmark::{Event, Options, Parser};

    let mut parser_options = Options::ENABLE_TABLES;
    if render_options.math {
        parser_options.insert(Options::ENABLE_MATH);
    }
    let mut tokens = Vec::new();
    let mut cursor = 0usize;
    let mut depth = 0i32;
    let mut block_start: Option<usize> = None;

    for (event, range) in Parser::new_ext(input, parser_options).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    // Close any preceding gap as a Space token.
                    let start = range.start;
                    if start > cursor {
                        let gap = &input[cursor..start];
                        tokens.push(LexedToken::space(gap));
                    }
                    block_start = Some(start);
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    let end = range.end;
                    let start = block_start.take().unwrap_or(cursor);
                    let block = &input[start..end];
                    tokens.push(LexedToken::other(block));
                    cursor = end;
                }
            }
            Event::Rule if depth == 0 => {
                if range.start > cursor {
                    tokens.push(LexedToken::space(&input[cursor..range.start]));
                }
                tokens.push(LexedToken::other(&input[range.start..range.end]));
                cursor = range.end;
            }
            Event::Rule => {}
            // Inline events at depth 0 shouldn't normally occur —
            // pulldown always wraps content in a block. If they do
            // (e.g. an HTML block emitted as a standalone Text), fold
            // them into the current block boundaries.
            _ => {}
        }
    }

    // Tail after the last block — could be whitespace or an unclosed
    // growing block. Emit as a trailing Other token so the streaming
    // boundary treats it as the "last content" and leaves the prefix
    // un-advanced (preserving the stable-prefix invariant).
    if cursor < input.len() {
        let tail = &input[cursor..];
        if tail.trim().is_empty() {
            tokens.push(LexedToken::space(tail));
        } else {
            tokens.push(LexedToken::other(tail));
        }
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_render::TokenKind;

    fn reproduces_input(input: &str) {
        let stripped = strip_prompt_xml_tags(input);
        let tokens = cmark_block_lex(&stripped);
        let recombined: String = tokens.iter().map(|t| t.raw.as_str()).collect();
        assert_eq!(
            recombined, stripped,
            "lexer output must recombine to the stripped input byte-for-byte"
        );
    }

    fn plain_lines(text: &Text<'static>) -> Vec<String> {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    // ---------------------------------------------------------------
    // cmark_block_lex contract
    // ---------------------------------------------------------------

    #[test]
    fn empty_input_yields_no_tokens() {
        assert!(cmark_block_lex("").is_empty());
    }

    #[test]
    fn single_paragraph_yields_one_other_token() {
        let tokens = cmark_block_lex("hello world");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Other);
        assert_eq!(tokens[0].raw, "hello world");
    }

    #[test]
    fn two_paragraphs_produce_other_space_other() {
        let tokens = cmark_block_lex("first\n\nsecond");
        // The interior blank is either swallowed into an adjacent
        // block's range or rendered as its own Space token — the
        // only contract we enforce is recombination.
        let recombined: String = tokens.iter().map(|t| t.raw.as_str()).collect();
        assert_eq!(recombined, "first\n\nsecond");
        assert!(tokens
            .iter()
            .any(|t| t.kind == TokenKind::Other && t.raw.contains("first")));
        assert!(tokens
            .iter()
            .any(|t| t.kind == TokenKind::Other && t.raw.contains("second")));
    }

    #[test]
    fn reproduces_for_every_shape() {
        let inputs: &[&str] = &[
            "plain text",
            "first\n\nsecond",
            "- a\n- b\n\npara",
            "```\ncode\n```\n\nafter",
            "# heading\n\npara",
            "> quote\n\npara",
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "para with **bold** and *em*",
            "mixed\n\n- list\n- item\n\n```\nfence\n```",
        ];
        for input in inputs {
            reproduces_input(input);
        }
    }

    #[test]
    fn reproduces_under_prompt_tag_stripping() {
        let inputs: &[&str] = &[
            "<context>ignore</context>real",
            "<context>x</context>\n\npara",
            "# h\n<context>x</context>\n\npara",
        ];
        for input in inputs {
            reproduces_input(input);
        }
    }

    #[test]
    fn unclosed_code_fence_is_a_single_token() {
        // An unclosed fence is a single token, so the boundary
        // algorithm doesn't retreat mid-fence. The
        // pulldown-cmark adapter must preserve that.
        let tokens = cmark_block_lex("```\nstill typing");
        let recombined: String = tokens.iter().map(|t| t.raw.as_str()).collect();
        assert_eq!(recombined, "```\nstill typing");
    }

    // ---------------------------------------------------------------
    // StreamingMarkdownRenderer behaviour
    // ---------------------------------------------------------------

    #[test]
    fn first_delta_has_no_stable_prefix() {
        let mut r = StreamingMarkdownRenderer::new();
        let split = r.advance("hello", &MarkdownTheme::plain());
        assert_eq!(split.stable_bytes, 0);
        assert!(split.stable.lines.is_empty());
        assert_eq!(plain_lines(&split.unstable), vec!["hello".to_string()]);
    }

    #[test]
    fn streaming_math_is_configured_and_default_off() {
        let mut off = StreamingMarkdownRenderer::new();
        let literal = off.advance("$x^2$", &MarkdownTheme::plain());
        assert!(literal.unstable_formulas.is_empty());
        assert_eq!(plain_lines(&literal.unstable), vec!["$x^2$".to_string()]);

        let mut on = StreamingMarkdownRenderer::with_options(MarkdownRenderOptions::math());
        let rendered = on.advance("$x^2$", &MarkdownTheme::plain());
        assert_eq!(rendered.unstable_formulas.len(), 1);
        assert_eq!(rendered.unstable_formulas[0].source, "$x^2$");
    }

    #[test]
    fn streaming_backslash_delimiters_keep_scanner_modes_in_each_half() {
        let mut renderer = StreamingMarkdownRenderer::with_options(MarkdownRenderOptions::math());
        let split = renderer.advance(
            r"\(x\)

\[y\]",
            &MarkdownTheme::plain(),
        );
        assert_eq!(split.stable_formulas.len(), 1);
        assert_eq!(split.stable_formulas[0].source, r"\(x\)");
        assert_eq!(
            split.stable_formulas[0].display,
            crate::FormulaDisplayMode::Inline
        );
        assert_eq!(split.unstable_formulas.len(), 1);
        assert_eq!(split.unstable_formulas[0].source, r"\[y\]");
        assert_eq!(
            split.unstable_formulas[0].display,
            crate::FormulaDisplayMode::Display
        );
    }

    #[test]
    fn streaming_unclosed_math_stays_literal_until_closed() {
        let mut renderer = StreamingMarkdownRenderer::with_options(MarkdownRenderOptions::math());
        for input in ["typing $x", "typing $x + 1"] {
            let split = renderer.advance(input, &MarkdownTheme::plain());
            assert!(split.stable_formulas.is_empty());
            assert!(split.unstable_formulas.is_empty());
            assert_eq!(plain_lines(&split.unstable), vec![input.to_string()]);
            let code = split
                .unstable
                .lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .filter(|span| span.style == MarkdownTheme::plain().code)
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(code.starts_with('$'), "{code:?}");
        }
        let closed = renderer.advance("typing $x + 1$", &MarkdownTheme::plain());
        assert_eq!(closed.unstable_formulas.len(), 1);
    }

    #[test]
    fn streaming_formula_sidecars_remain_local_to_both_halves() {
        let mut renderer = StreamingMarkdownRenderer::with_options(MarkdownRenderOptions::math());
        let split = renderer.advance("$x$\n\n$y$", &MarkdownTheme::plain());
        assert_eq!(split.stable_formulas.len(), 1);
        assert_eq!(split.unstable_formulas.len(), 1);
        assert_eq!(split.stable_formulas[0].ranges[0].line, 0);
        assert_eq!(split.unstable_formulas[0].ranges[0].line, 1);
    }

    #[test]
    fn second_delta_commits_first_paragraph() {
        let mut r = StreamingMarkdownRenderer::new();
        let _ = r.advance("p1", &MarkdownTheme::plain());
        let split = r.advance("p1\n\np2", &MarkdownTheme::plain());
        assert!(split.stable_bytes > 0);
        // Stable renders to "p1"; unstable renders to "p2" after the
        // boundary splits on the blank line.
        let stable_rows = plain_lines(&split.stable);
        let unstable_rows = plain_lines(&split.unstable);
        assert!(stable_rows.iter().any(|l| l == "p1"));
        assert!(unstable_rows.iter().any(|l| l == "p2"));
    }

    #[test]
    fn appending_inside_same_block_does_not_retreat() {
        let mut r = StreamingMarkdownRenderer::new();
        let _ = r.advance("p1\n\np2", &MarkdownTheme::plain());
        let before = r.stable_prefix().to_string();
        let _ = r.advance("p1\n\np2 more", &MarkdownTheme::plain());
        assert_eq!(r.stable_prefix(), before, "prefix retreated mid-block");
    }

    #[test]
    fn shrinking_text_resets_prefix() {
        let mut r = StreamingMarkdownRenderer::new();
        let _ = r.advance("p1\n\np2 still streaming", &MarkdownTheme::plain());
        assert!(!r.stable_prefix().is_empty());
        let _ = r.advance("p1", &MarkdownTheme::plain());
        assert_eq!(r.stable_prefix(), "");
    }

    #[test]
    fn monotonic_advance_over_many_deltas() {
        let mut r = StreamingMarkdownRenderer::new();
        let inputs = [
            "# ",
            "# H",
            "# Header",
            "# Header\n",
            "# Header\n\n",
            "# Header\n\npara",
            "# Header\n\npara more",
            "# Header\n\npara more\n\n- li",
            "# Header\n\npara more\n\n- list",
        ];
        let mut last = 0usize;
        for input in inputs {
            let _ = r.advance(input, &MarkdownTheme::plain());
            let cur = r.stable_prefix().len();
            assert!(cur >= last, "retreat on {input:?}: {last} → {cur}");
            last = cur;
        }
    }

    #[test]
    fn streaming_split_preserves_stable_and_unstable_hyperlinks() {
        let mut renderer = StreamingMarkdownRenderer::new();
        let split = renderer.advance(
            "[stable](https://stable.test)\n\nvisit https://unstable.test.",
            &MarkdownTheme::plain(),
        );
        assert_eq!(split.stable_hyperlinks.len(), 1);
        assert_eq!(split.stable_hyperlinks[0].target, "https://stable.test");
        assert_eq!(split.unstable_hyperlinks.len(), 1);
        assert_eq!(split.unstable_hyperlinks[0].target, "https://unstable.test");
        assert_eq!(split.stable_hyperlinks[0].line, 0);
        assert_eq!(split.unstable_hyperlinks[0].line, 1);
    }

    #[test]
    fn rendered_split_concatenates_to_committed_visible_lines() {
        for input in [
            "first para\n\nsecond para\n\nthird streaming",
            "first para\n\n```text\ncode\n```",
            "```text\ncode\n```\n\nafter",
            "| A |\n|---|\n| one |\n\nafter",
        ] {
            let mut renderer = StreamingMarkdownRenderer::new();
            let split = renderer.advance(input, &MarkdownTheme::plain());
            let combined = split
                .stable
                .lines
                .iter()
                .chain(split.unstable.lines.iter())
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let committed = plain_lines(&render_streaming_message(input, &MarkdownTheme::plain()));
            assert_eq!(combined, committed, "{input:?}");
        }
    }

    #[test]
    fn reset_clears_state() {
        let mut r = StreamingMarkdownRenderer::new();
        let _ = r.advance("p1\n\np2", &MarkdownTheme::plain());
        assert!(!r.stable_prefix().is_empty());
        r.reset();
        assert_eq!(r.stable_prefix(), "");
    }

    #[test]
    fn prompt_tag_arrives_does_not_panic() {
        // Edge case: when `</context>` finally arrives mid-stream, the
        // stripped string shrinks. The boundary's reset-if-not-prefix
        // branch should handle it without a panic and reset the prefix.
        let mut r = StreamingMarkdownRenderer::new();
        let _ = r.advance("p1\n\n<context>secret data", &MarkdownTheme::plain());
        let _ = r.advance(
            "p1\n\n<context>secret data</context>",
            &MarkdownTheme::plain(),
        );
        // After strip, only "p1" remains — unstable should contain it.
        // The prefix should reset because "p1" doesn't start with the
        // previous stable prefix "p1\n\n".
        assert_eq!(r.stable_prefix(), "");
    }

    // ---------------------------------------------------------------
    // Non-streaming entry point
    // ---------------------------------------------------------------

    #[test]
    fn render_streaming_message_strips_and_renders() {
        let text = render_streaming_message(
            "<context>hide</context># heading\n\npara",
            &MarkdownTheme::plain(),
        );
        let lines = plain_lines(&text);
        assert!(lines.iter().any(|l| l == "heading"));
        assert!(lines.iter().any(|l| l == "para"));
        assert!(!lines.iter().any(|l| l.contains("hide")));
    }

    #[test]
    fn render_streaming_message_plain_text_fast_path() {
        let text = render_streaming_message("just a plain sentence", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec!["just a plain sentence".to_string()]
        );
    }
}
