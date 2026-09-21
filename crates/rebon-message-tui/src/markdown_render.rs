//! Markdown → `ratatui::Text<'static>` renderer for the message stack.
//!
//! ## How it renders
//!
//! Lex with `pulldown-cmark` and walk each token into a styled
//! `ratatui::Span`, then assemble the spans into a `Text<'static>` the
//! widget subtree paints directly.
//!
//! Strikethrough is dropped: this module opts out of `pulldown-cmark`'s
//! strikethrough extension to preserve the
//! "`~100`-as-approximate, not strikethrough" rule.
//!
//! ## What this module owns
//!
//! * A small [`MarkdownTheme`] style bag derived from
//!   [`crate::MessagesRenderTheme`] — the caller's own
//!   styling is not lost, only extended with the markdown-specific
//!   additions (code span background, heading underline, etc.).
//! * [`render_markdown`] — the plain entry point returning styled text.
//! * [`render_markdown_annotated`] — the rich entry point returning styled
//!   text plus hyperlink byte ranges. Both strip the four reserved wrapper
//!   tags before parsing.
//! * [`render_markdown_blocks`] and [`render_markdown_blocks_annotated`] —
//!   the same renderers without outer `strip_prompt_xml_tags`, for callers
//!   that already stripped input (notably [`crate::streaming_markdown`]).
//!
//! ## What is deliberately kept simple
//!
//! * **Tables** — rendered through `rebon-render`'s boxed table
//!   layout so columns adapt to the available terminal width and fall
//!   back to vertical key/value rows when the grid would overflow.
//! * **Syntax highlighting** — a lightweight built-in highlighter adds
//!   visible code-block background plus keyword/string/comment/number
//!   spans. A full external highlighter pipeline is not yet
//!   implemented, but fenced blocks no longer render as unstyled text.
//! * **Hyperlinks** — markdown links and bare `http`/`https` URLs are
//!   styled and returned as byte ranges in the final visible text. The
//!   widget layer can use those annotations for hit-testing without
//!   embedding terminal escape sequences in ratatui spans.

use std::{
    ops::Range,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::MessagesRenderTheme;
use pulldown_cmark::{
    Alignment as CmarkAlignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options,
    Parser, Tag, TagEnd,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use rebon_math::{
    render_formula, FormulaColor, FormulaDisplayMode as CoreFormulaDisplayMode, FormulaRenderError,
    FormulaRenderOptions, FORMULA_CELL_HEIGHT_PX, FORMULA_CELL_WIDTH_PX,
};
pub use rebon_math::{
    FormulaAsset, FormulaBitmap, MAX_FORMULA_BITMAP_PIXELS, MAX_FORMULA_SOURCE_BYTES,
    MAX_FORMULA_TERMINAL_ROWS,
};
use rebon_render::{
    scan_math_fragments, strip_prompt_xml_tags,
    table_layout::{compute_available_width, compute_column_widths, MIN_COLUMN_WIDTH},
    table_render::{render_horizontal_table, CellLines, TableInput},
    Alignment as TableAlignment, MathDelimiter, MathDisplayMode, MathFragment,
};
use rebon_width::WidthStr;
use unicode_segmentation::UnicodeSegmentation;

/// Default content width used by callers that do not have a live terminal
/// width yet. Widget-backed assistant text passes the actual content width.
pub const DEFAULT_MARKDOWN_TERMINAL_WIDTH: usize = 80;

const CURRENCY_DOLLAR_SENTINEL: u8 = b'~';

/// Opt-in extensions for the Markdown renderer.
///
/// The default deliberately leaves math disabled so existing `$`-heavy text
/// keeps its historical literal rendering unless the caller explicitly opts in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkdownRenderOptions {
    /// Parse `$...$`, `$$...$$`, `\(...\)`, and `\[...\]` formulas.
    pub math: bool,
}

impl MarkdownRenderOptions {
    /// Construct options with formula parsing enabled.
    pub const fn math() -> Self {
        Self { math: true }
    }

    /// Return a copy with formula parsing set to `enabled`.
    pub const fn with_math(mut self, enabled: bool) -> Self {
        self.math = enabled;
        self
    }
}

/// Whether a rendered formula came from inline or display delimiters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormulaDisplayMode {
    /// Formula parsed from `$...$` or `\(...\)`.
    Inline,
    /// Formula parsed from `$$...$$` or `\[...\]`.
    Display,
}

/// One visible fallback segment occupied by a formula in rendered text.
///
/// Byte offsets address the concatenated UTF-8 contents of the logical line,
/// using the same coordinate system as [`HyperlinkRange`]. Multi-row display
/// formulas contain one region per terminal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaRange {
    /// Zero-based logical line index.
    pub line: usize,
    /// Inclusive UTF-8 byte offset within the logical line.
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset within the logical line.
    pub end_byte: usize,
}

/// Formula metadata and assets carried beside [`RenderedMarkdown::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFormula {
    /// Original Markdown source, including its math delimiters.
    pub source: String,
    /// Formula body passed to RaTeX, without Markdown delimiters.
    pub expression: String,
    /// Inline versus display parsing mode.
    pub display: FormulaDisplayMode,
    /// Visible regions occupied by the Unicode fallback.
    pub ranges: Vec<FormulaRange>,
    /// Number of terminal columns reserved by the fallback.
    pub terminal_columns: u16,
    /// Number of terminal rows reserved by the fallback.
    pub terminal_rows: u16,
    /// Deterministic Unicode half-block representation used for layout.
    pub fallback: Text<'static>,
    /// SVG and RGBA assets for later native graphics overlays.
    pub asset: FormulaAsset,
}

/// One clickable hyperlink segment in a rendered markdown line.
///
/// A logical link that crosses a soft or hard break is represented by one
/// range per visible line. Byte offsets are measured in the concatenated UTF-8
/// contents of the corresponding [`ratatui::text::Line`], after visible
/// prefixes such as blockquote gutters have been inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkRange {
    /// Zero-based line index in [`RenderedMarkdown::text`].
    pub line: usize,
    /// Inclusive UTF-8 byte offset within the visible line.
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset within the visible line.
    pub end_byte: usize,
    /// Link destination used for navigation when this range is activated.
    pub target: String,
}

/// Styled markdown text together with hyperlink hit-test annotations.
///
/// Every hyperlink range addresses the final visible [`Text`] stored in
/// [`Self::text`]. The annotations contain no terminal escape sequences, so
/// callers may paint the text normally and handle link interaction separately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedMarkdown {
    /// Final styled text produced by the markdown renderer.
    pub text: Text<'static>,
    /// Clickable hyperlink segments in the final visible text.
    pub hyperlinks: Vec<HyperlinkRange>,
    /// Successfully rendered formulas and their visible/graphics sidecars.
    pub formulas: Vec<RenderedFormula>,
}

/// Extended style bag for markdown rendering.
///
/// Derived from [`MessagesRenderTheme`] via [`MarkdownTheme::from_messages`],
/// which reuses that theme's `text` and `dim` styles and builds the rest
/// on top of them:
///
/// | field | built from | purpose |
/// |---|---|---|
/// | `text` | `theme.text` | paragraph body / list body |
/// | `dim` | `theme.dim` | secondary text and the `---` rule |
/// | `strong` | `text` + bold | `**bold**` |
/// | `emphasis` | `text` + italic | `*em*` / `_em_` |
/// | `link` | `theme.accent` + underlined | markdown-link labels and bare URLs |
/// | `code` | `theme.accent` | `` `inline code` `` |
/// | `code_block` | `theme.text` | ` ```code``` ` |
/// | `heading` | `text` + bold + underlined | h1 |
/// | `heading_h2` | `text` + bold | h2+ |
/// | `blockquote_bar` | `theme.dim` | `│` prefix |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownTheme {
    /// Plain text style.
    pub text: Style,
    /// Dim/secondary style used for rule lines and secondary annotations.
    pub dim: Style,
    /// Bold style for `**strong**`.
    pub strong: Style,
    /// Italic style for `*emphasis*`.
    pub emphasis: Style,
    /// Hyperlink-label and bare-URL style.
    pub link: Style,
    /// Inline code (`` `foo` ``) style.
    pub code: Style,
    /// Fenced/indented code block style.
    pub code_block: Style,
    /// Style for the first heading level (h1) — bold + underlined.
    pub heading: Style,
    /// Style for h2+.
    pub heading_h2: Style,
    /// Style for the `│` blockquote gutter.
    pub blockquote_bar: Style,
}

impl MarkdownTheme {
    /// Build from a caller-provided [`MessagesRenderTheme`]. Reuses the
    /// theme's `text`/`dim` so markdown spans inherit the surrounding
    /// message palette; adds markdown-specific modifiers on top.
    pub fn from_messages(theme: &MessagesRenderTheme) -> Self {
        Self {
            text: theme.text,
            dim: theme.dim,
            strong: theme.text.add_modifier(Modifier::BOLD),
            emphasis: theme.text.add_modifier(Modifier::ITALIC),
            link: theme.accent.add_modifier(Modifier::UNDERLINED),
            code: theme.accent,
            code_block: theme.text,
            heading: theme
                .text
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
            heading_h2: theme.text.add_modifier(Modifier::BOLD),
            blockquote_bar: theme.dim,
        }
    }

    /// Deterministic no-style theme for tests / ANSI-off surfaces.
    pub const fn plain() -> Self {
        Self {
            text: Style::new(),
            dim: Style::new(),
            strong: Style::new().add_modifier(Modifier::BOLD),
            emphasis: Style::new().add_modifier(Modifier::ITALIC),
            link: Style::new()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED),
            code: Style::new().bg(Color::Indexed(236)),
            code_block: Style::new().bg(Color::Indexed(236)),
            heading: Style::new()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
            heading_h2: Style::new().add_modifier(Modifier::BOLD),
            blockquote_bar: Style::new(),
        }
    }
}

/// Blockquote gutter glyph, repeated once per nesting level before every
/// quoted line. Kept in-module
/// to avoid pulling another constant seam just for one character.
pub const BLOCKQUOTE_BAR: &str = "│ ";

/// Entry point used by non-streaming callers. Strips the reserved
/// wrapper tags and returns only the styled text.
///
/// Use [`render_markdown_annotated`] when hyperlink byte ranges are needed.
pub fn render_markdown(src: &str, theme: &MarkdownTheme) -> Text<'static> {
    render_markdown_with_options(src, theme, MarkdownRenderOptions::default())
}

/// Render Markdown with explicit opt-in extension options.
pub fn render_markdown_with_options(
    src: &str,
    theme: &MarkdownTheme,
    render_options: MarkdownRenderOptions,
) -> Text<'static> {
    render_markdown_annotated_with_options(src, theme, render_options).text
}

/// Render markdown with hyperlink annotations at the default content width.
///
/// Reserved prompt wrapper tags are stripped before parsing. Use
/// [`render_markdown_annotated_with_width`] when the live content width is
/// available.
pub fn render_markdown_annotated(src: &str, theme: &MarkdownTheme) -> RenderedMarkdown {
    render_markdown_annotated_with_options(src, theme, MarkdownRenderOptions::default())
}

/// Render Markdown annotations with explicit opt-in extension options.
pub fn render_markdown_annotated_with_options(
    src: &str,
    theme: &MarkdownTheme,
    render_options: MarkdownRenderOptions,
) -> RenderedMarkdown {
    render_markdown_annotated_with_width_and_options(
        src,
        theme,
        DEFAULT_MARKDOWN_TERMINAL_WIDTH,
        render_options,
    )
}

/// Width-aware variant of [`render_markdown`] returning only styled text.
///
/// Tables use `terminal_width` to choose horizontal versus vertical layout.
pub fn render_markdown_with_width(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
) -> Text<'static> {
    render_markdown_with_width_and_options(
        src,
        theme,
        terminal_width,
        MarkdownRenderOptions::default(),
    )
}

/// Width-aware Markdown rendering with explicit extension options.
pub fn render_markdown_with_width_and_options(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
    render_options: MarkdownRenderOptions,
) -> Text<'static> {
    render_markdown_annotated_with_width_and_options(src, theme, terminal_width, render_options)
        .text
}

/// Width-aware markdown renderer with hyperlink annotations.
///
/// The returned ranges address the final visible text after reserved prompt
/// wrappers have been removed and block prefixes have been inserted.
pub fn render_markdown_annotated_with_width(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
) -> RenderedMarkdown {
    render_markdown_annotated_with_width_and_options(
        src,
        theme,
        terminal_width,
        MarkdownRenderOptions::default(),
    )
}

/// Width-aware annotated Markdown rendering with explicit extension options.
pub fn render_markdown_annotated_with_width_and_options(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
    render_options: MarkdownRenderOptions,
) -> RenderedMarkdown {
    let stripped = strip_prompt_xml_tags(src);
    render_markdown_blocks_annotated_with_width_and_options(
        &stripped,
        theme,
        terminal_width,
        render_options,
    )
}

/// Render already-stripped markdown and return only styled text.
///
/// This entry point is used by streaming callers that preprocess the
/// reserved prompt wrappers themselves.
pub fn render_markdown_blocks(src: &str, theme: &MarkdownTheme) -> Text<'static> {
    render_markdown_blocks_with_options(src, theme, MarkdownRenderOptions::default())
}

/// Render already-stripped Markdown with explicit extension options.
pub fn render_markdown_blocks_with_options(
    src: &str,
    theme: &MarkdownTheme,
    render_options: MarkdownRenderOptions,
) -> Text<'static> {
    render_markdown_blocks_annotated_with_options(src, theme, render_options).text
}

/// Render already-stripped markdown with hyperlink annotations at the default
/// content width.
pub fn render_markdown_blocks_annotated(src: &str, theme: &MarkdownTheme) -> RenderedMarkdown {
    render_markdown_blocks_annotated_with_options(src, theme, MarkdownRenderOptions::default())
}

/// Render already-stripped annotations with explicit extension options.
pub fn render_markdown_blocks_annotated_with_options(
    src: &str,
    theme: &MarkdownTheme,
    render_options: MarkdownRenderOptions,
) -> RenderedMarkdown {
    render_markdown_blocks_annotated_with_width_and_options(
        src,
        theme,
        DEFAULT_MARKDOWN_TERMINAL_WIDTH,
        render_options,
    )
}

/// Width-aware variant of [`render_markdown_blocks`].
pub fn render_markdown_blocks_with_width(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
) -> Text<'static> {
    render_markdown_blocks_with_width_and_options(
        src,
        theme,
        terminal_width,
        MarkdownRenderOptions::default(),
    )
}

/// Width-aware stripped Markdown rendering with explicit extension options.
pub fn render_markdown_blocks_with_width_and_options(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
    render_options: MarkdownRenderOptions,
) -> Text<'static> {
    render_markdown_blocks_annotated_with_width_and_options(
        src,
        theme,
        terminal_width,
        render_options,
    )
    .text
}

/// Render already-stripped markdown at `terminal_width`, preserving hyperlink
/// annotations for the final visible text.
pub fn render_markdown_blocks_annotated_with_width(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
) -> RenderedMarkdown {
    render_markdown_blocks_annotated_with_width_and_options(
        src,
        theme,
        terminal_width,
        MarkdownRenderOptions::default(),
    )
}

/// Render already-stripped Markdown at `terminal_width` with explicit options.
pub fn render_markdown_blocks_annotated_with_width_and_options(
    src: &str,
    theme: &MarkdownTheme,
    terminal_width: usize,
    render_options: MarkdownRenderOptions,
) -> RenderedMarkdown {
    if src.is_empty() {
        return RenderedMarkdown::default();
    }
    let mut parser_options = Options::ENABLE_TABLES;
    if render_options.math {
        parser_options.insert(Options::ENABLE_MATH);
    }
    let math_fragments = if render_options.math {
        scan_math_fragments(src)
    } else {
        Vec::new()
    };
    let parser_source = render_options
        .math
        .then(|| prepare_math_parser_source(src, &math_fragments))
        .flatten();
    let parser_source = parser_source.as_deref().unwrap_or(src);
    let events = Parser::new_ext(parser_source, parser_options)
        .into_offset_iter()
        .map(|(event, range)| {
            let event = restore_currency_dollars(event, &range, src, parser_source);
            (event, range)
        })
        .collect::<Vec<_>>();
    let literal_math_ranges = if render_options.math {
        find_literal_math_ranges(parser_source, &events)
    } else {
        Vec::new()
    };
    let mut renderer = Renderer::new(
        *theme,
        terminal_width,
        render_options,
        literal_math_ranges.clone(),
        math_fragments,
    );
    let mut emitted_literal_ranges = vec![false; literal_math_ranges.len()];
    for (event, range) in events {
        let literal = literal_math_ranges.iter().enumerate().find(|(_, literal)| {
            ranges_overlap(literal, &range) && event_is_inline_content(&event)
        });
        if let Some((index, literal)) = literal {
            if !emitted_literal_ranges[index] {
                let prefix_end = literal.start.min(range.end);
                if range.start < prefix_end {
                    match &event {
                        Event::Text(text) if text.len() == range.len() => {
                            renderer.push_text(&text[..prefix_end - range.start]);
                        }
                        _ => renderer.push_text(&src[range.start..prefix_end]),
                    }
                }
                renderer.push_literal_math_text(&src[literal.clone()]);
                emitted_literal_ranges[index] = true;
            }
            continue;
        }
        renderer.handle(event, range, src);
    }
    renderer.finish()
}

fn prepare_math_parser_source(source: &str, fragments: &[MathFragment]) -> Option<String> {
    let masked = mask_currency_dollars(source);
    let mut changed = masked.is_some();
    let mut bytes = masked.unwrap_or_else(|| source.to_string()).into_bytes();
    let protected = if fragments.iter().any(|fragment| {
        matches!(
            fragment.delimiter,
            MathDelimiter::Parentheses | MathDelimiter::Brackets
        )
    }) {
        Parser::new_ext(source, Options::ENABLE_TABLES)
            .into_offset_iter()
            .filter_map(|(event, range)| match event {
                Event::Start(Tag::Link { .. } | Tag::Image { .. })
                | Event::InlineHtml(_)
                | Event::Html(_) => Some(range),
                _ => None,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for fragment in fragments {
        if !matches!(
            fragment.delimiter,
            MathDelimiter::Parentheses | MathDelimiter::Brackets
        ) || protected
            .iter()
            .any(|range| ranges_overlap(range, &fragment.source_range))
        {
            continue;
        }
        let start = fragment.source_range.start;
        let end = fragment.source_range.end;
        bytes[start..start + 2].copy_from_slice(b"$$");
        bytes[end - 2..end].copy_from_slice(b"$$");
        changed = true;
    }
    changed.then(|| String::from_utf8(bytes).expect("ASCII replacements preserve UTF-8"))
}

fn mask_currency_dollars(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut masked = source.as_bytes().to_vec();
    let mut changed = false;
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] != b'$'
            || is_backslash_escaped(bytes, index)
            || !bytes[index + 1].is_ascii_digit()
        {
            index += 1;
            continue;
        }

        let next_dollar = bytes[index + 1..]
            .iter()
            .position(|byte| *byte == b'$')
            .map(|offset| index + 1 + offset);
        let looks_like_math = next_dollar.is_some_and(|end| {
            let candidate = &source[index + 1..end];
            !candidate.chars().any(char::is_whitespace)
                || candidate.bytes().any(|byte| {
                    matches!(
                        byte,
                        b'+' | b'-'
                            | b'*'
                            | b'/'
                            | b'^'
                            | b'_'
                            | b'='
                            | b'\\'
                            | b'{'
                            | b'}'
                            | b'['
                            | b']'
                            | b'('
                            | b')'
                    )
                })
        });
        if !looks_like_math {
            masked[index] = CURRENCY_DOLLAR_SENTINEL;
            changed = true;
        }
        index += 1;
    }

    changed.then(|| String::from_utf8(masked).expect("ASCII replacement preserves UTF-8"))
}

fn restore_currency_dollars<'a>(
    event: Event<'a>,
    range: &Range<usize>,
    source: &str,
    parser_source: &str,
) -> Event<'a> {
    fn restore<'a>(
        text: pulldown_cmark::CowStr<'a>,
        range: &Range<usize>,
        source: &str,
        parser_source: &str,
    ) -> pulldown_cmark::CowStr<'a> {
        if !text.as_bytes().contains(&CURRENCY_DOLLAR_SENTINEL) {
            return text;
        }

        let masked_positions = (range.start..range.end)
            .filter(|index| {
                source.as_bytes().get(*index) == Some(&b'$')
                    && parser_source.as_bytes().get(*index) == Some(&CURRENCY_DOLLAR_SENTINEL)
            })
            .collect::<Vec<_>>();
        if masked_positions.is_empty() {
            return text;
        }

        let mut restored = text.to_string();
        if restored.len() == range.len() {
            let mut bytes = restored.into_bytes();
            for position in masked_positions {
                bytes[position - range.start] = b'$';
            }
            restored = String::from_utf8(bytes).expect("ASCII replacement preserves UTF-8");
        } else {
            let parser_fragment = &parser_source[range.clone()];
            let relative_start = parser_fragment
                .match_indices(restored.as_str())
                .map(|(start, _)| start)
                .find(|start| {
                    (0..restored.len()).any(|offset| {
                        let position = range.start + start + offset;
                        source.as_bytes().get(position) == Some(&b'$')
                            && parser_source.as_bytes().get(position)
                                == Some(&CURRENCY_DOLLAR_SENTINEL)
                    })
                });
            if let Some(relative_start) = relative_start {
                let mut bytes = restored.into_bytes();
                for (offset, byte) in bytes.iter_mut().enumerate() {
                    let position = range.start + relative_start + offset;
                    if *byte == CURRENCY_DOLLAR_SENTINEL
                        && source.as_bytes().get(position) == Some(&b'$')
                        && parser_source.as_bytes().get(position) == Some(&CURRENCY_DOLLAR_SENTINEL)
                    {
                        *byte = b'$';
                    }
                }
                restored = String::from_utf8(bytes).expect("ASCII replacement preserves UTF-8");
            }
        }
        restored.into()
    }

    match event {
        Event::Text(text) => Event::Text(restore(text, range, source, parser_source)),
        Event::Code(text) => Event::Code(restore(text, range, source, parser_source)),
        Event::Html(text) => Event::Html(restore(text, range, source, parser_source)),
        Event::InlineHtml(text) => Event::InlineHtml(restore(text, range, source, parser_source)),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: restore(dest_url, range, source, parser_source),
            title: restore(title, range, source, parser_source),
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: restore(dest_url, range, source, parser_source),
            title: restore(title, range, source, parser_source),
            id,
        }),
        other => other,
    }
}

/// Active inline-style frame pushed on `Start(Emphasis)` / `Start(Strong)` /
/// `Start(Link { .. })` and popped on the matching `End`. The renderer
/// composes these by modifier-OR so nested `**_bold italic_**` works.
#[derive(Debug, Clone, Copy)]
enum InlineFrame {
    Emphasis,
    Strong,
    Link,
    CodeBlock,
}

#[derive(Debug, Clone)]
struct ActiveLink {
    target: String,
    has_visible_text: bool,
}

#[derive(Debug, Clone)]
struct CurrentHyperlinkRange {
    start_byte: usize,
    end_byte: usize,
    target: String,
}

#[derive(Debug, Clone, Copy)]
struct CurrentFormulaRange {
    formula: usize,
    start_byte: usize,
    end_byte: usize,
}

struct Renderer {
    theme: MarkdownTheme,
    lines: Vec<Line<'static>>,
    /// Hyperlink ranges kept in lockstep with `lines` until final flattening.
    line_hyperlinks: Vec<Vec<HyperlinkRange>>,
    current: Vec<Span<'static>>,
    /// Link ranges for `current`, before a blockquote prefix is inserted.
    current_hyperlinks: Vec<CurrentHyperlinkRange>,
    /// Formula ranges for `current`, before blockquote prefixes are inserted.
    current_formula_ranges: Vec<CurrentFormulaRange>,
    /// Successful formula sidecars accumulated during rendering.
    formulas: Vec<RenderedFormula>,
    /// Ranges that look like math delimiters but were intentionally left
    /// literal by pulldown-cmark (notably unclosed streaming input).
    literal_math_ranges: Vec<Range<usize>>,
    /// Closed fragments recognized by the shared scanner. These map normalized
    /// backslash-delimited parser events to their original ranges and modes.
    math_fragments: Vec<MathFragment>,
    /// Explicit extension configuration for this render.
    render_options: MarkdownRenderOptions,
    /// List marker for a newly-opened item that has not yet received
    /// real line content. Keeping this out of `current` prevents
    /// `Start(Paragraph)`/`open_block()` from flushing `"1. "` as its
    /// own visual line for loose ordered-list input.
    pending_list_marker: Option<Span<'static>>,
    inline_stack: Vec<InlineFrame>,
    /// One entry per `List` level; `Some(start)` for ordered lists,
    /// `None` for bulleted. The last entry is the active list; its
    /// `start` gets incremented after each `End(Item)`.
    list_stack: Vec<Option<u64>>,
    /// `true` while inside `Start(BlockQuote)` / `End(BlockQuote)`.
    /// Each new line started inside a blockquote is prefixed with the
    /// bar span.
    blockquote_depth: usize,
    /// When inside a code block, collect `Text(...)` events into
    /// per-line styled spans unchanged rather than routing them through
    /// the inline-style stack.
    in_code_block: bool,
    /// Active markdown link. Its visible label may span multiple events and
    /// rendered lines; each line receives its own final range.
    active_link: Option<ActiveLink>,
    /// `true` once we've emitted content; used to decide whether a
    /// block separator (blank line) is needed before the next block.
    has_emitted_block: bool,
    /// Skip the automatic blank separator before the next block.
    suppress_next_block_separator: bool,
    /// Keep heading level so nested styles compose with the heading
    /// style on close.
    heading_level: Option<HeadingLevel>,
    /// Table rows accumulated inside `Start(Table)` → `End(Table)`. Cells keep
    /// both plain text for width calculations and spans for future styling.
    table_rows: Vec<Vec<TableCell>>,
    /// Current table cell being accumulated.
    table_cell_buf: Option<String>,
    /// Available terminal/content width used for table layout.
    terminal_width: usize,
    /// Fenced-code language info for the active code block, if any.
    code_block_language: Option<String>,
    /// True when the current code-block line already has styled spans.
    code_block_line_has_content: bool,
    /// True between `Start(Table)` and `End(Table)`.
    in_table: bool,
    /// Alignment markers from the current markdown table.
    table_align: Vec<CmarkAlignment>,
    /// Current table row being accumulated.
    table_current_row: Option<Vec<TableCell>>,
    /// Current table cell being accumulated with styled spans.
    table_current_cell: Option<TableCell>,
    /// Once an unclosed/invalid `$` starts in a table cell, preserve the
    /// remainder of that cell with code styling.
    table_literal_math: bool,
}

impl Renderer {
    fn new(
        theme: MarkdownTheme,
        terminal_width: usize,
        render_options: MarkdownRenderOptions,
        literal_math_ranges: Vec<Range<usize>>,
        math_fragments: Vec<MathFragment>,
    ) -> Self {
        Self {
            theme,
            terminal_width: terminal_width.max(1),
            lines: Vec::new(),
            line_hyperlinks: Vec::new(),
            current: Vec::new(),
            current_hyperlinks: Vec::new(),
            current_formula_ranges: Vec::new(),
            formulas: Vec::new(),
            literal_math_ranges,
            math_fragments,
            render_options,
            pending_list_marker: None,
            inline_stack: Vec::new(),
            list_stack: Vec::new(),
            blockquote_depth: 0,
            in_code_block: false,
            code_block_language: None,
            code_block_line_has_content: false,
            active_link: None,
            has_emitted_block: false,
            suppress_next_block_separator: false,
            heading_level: None,
            table_rows: Vec::new(),
            table_cell_buf: None,
            in_table: false,
            table_align: Vec::new(),
            table_current_row: None,
            table_current_cell: None,
            table_literal_math: false,
        }
    }

    fn finish(mut self) -> RenderedMarkdown {
        if self.pending_list_marker.is_some() {
            self.flush_pending_list_marker();
        }
        if !self.current.is_empty() {
            self.flush_line();
        }
        // Drop a trailing blank separator if one snuck in (the
        // rendered text is trimmed at the end).
        while matches!(self.lines.last(), Some(l) if line_is_blank(l)) {
            self.lines.pop();
            self.line_hyperlinks.pop();
        }
        RenderedMarkdown {
            text: Text::from(self.lines),
            hyperlinks: self.line_hyperlinks.into_iter().flatten().collect(),
            formulas: self.formulas,
        }
    }

    fn handle(&mut self, event: Event<'_>, source_range: Range<usize>, source: &str) {
        if self.in_table && self.handle_table_event(&event, &source[source_range.clone()]) {
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_source_text(&text, source_range),
            Event::Code(code) => {
                let style = if self.active_link.is_some() {
                    self.theme.link
                } else {
                    self.theme.code
                };
                self.push_span(code.to_string(), style);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                // Strip common wrapper tags we don't render; drop raw
                // HTML elsewhere.
                let t = html.to_string();
                if t.trim().is_empty() {
                    return;
                }
                // Keep <br> as a hard break to be nice to free-form
                // assistant output that sneaks one in.
                if t.trim_end().eq_ignore_ascii_case("<br>")
                    || t.trim_end().eq_ignore_ascii_case("<br/>")
                    || t.trim_end().eq_ignore_ascii_case("<br />")
                {
                    self.flush_line();
                }
            }
            Event::SoftBreak => {
                // A soft break inside a paragraph maps to a single
                // newline, the same as a hard one.
                self.flush_line();
            }
            Event::HardBreak => self.flush_line(),
            Event::Rule => self.push_rule(),
            Event::TaskListMarker(checked) => {
                let glyph = if checked { "[x] " } else { "[ ] " };
                self.flush_pending_list_marker();
                self.push_span(glyph.to_string(), self.theme.text);
            }
            Event::FootnoteReference(label) => {
                self.push_span(format!("[^{label}]"), self.theme.dim);
            }
            Event::InlineMath(math) => {
                self.push_parsed_formula(&math, source_range, source, FormulaDisplayMode::Inline)
            }
            Event::DisplayMath(math) => {
                self.push_parsed_formula(&math, source_range, source, FormulaDisplayMode::Display)
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.open_block();
            }
            Tag::Heading { level, .. } => {
                self.open_block();
                self.heading_level = Some(level);
            }
            Tag::BlockQuote(kind) => {
                let follows_previous_block = self.open_block();
                if follows_previous_block {
                    self.suppress_next_block_separator = true;
                }
                self.blockquote_depth += 1;
                // `> [!NOTE]`-style callouts: surface the kind as a
                // dim inline badge on the first blockquote line.
                if let Some(kind) = kind {
                    let label = blockquote_kind_label(kind);
                    self.current
                        .push(Span::styled(label.to_string(), self.theme.dim));
                    self.current.push(Span::raw(" "));
                }
            }
            Tag::CodeBlock(kind) => {
                self.open_tight_block();
                self.in_code_block = true;
                self.code_block_language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().map(str::to_string)
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code_block_line_has_content = false;
                self.inline_stack.push(InlineFrame::CodeBlock);
            }
            Tag::HtmlBlock => {
                self.open_block();
            }
            Tag::List(start) => {
                self.open_block();
                self.list_stack.push(start);
            }
            Tag::Item => self.begin_list_item(),
            Tag::FootnoteDefinition(label) => {
                self.open_block();
                self.current
                    .push(Span::styled(format!("[^{label}]: "), self.theme.dim));
            }
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                self.open_block();
            }
            Tag::Table(alignments) => {
                self.open_block();
                self.in_table = true;
                self.table_align = alignments;
                self.table_rows.clear();
                self.table_current_row = None;
                self.table_current_cell = None;
                self.table_cell_buf = None;
                self.table_literal_math = false;
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {
                // handled in handle_table_event
            }
            Tag::Emphasis => self.inline_stack.push(InlineFrame::Emphasis),
            Tag::Strong => self.inline_stack.push(InlineFrame::Strong),
            Tag::Strikethrough => {
                // Strikethrough is not enabled in the `pulldown`
                // Options bitmask we pass, so this branch shouldn't normally
                // fire — but the grammar still recognises it internally in
                // some versions. Treat as pass-through.
            }
            Tag::Link { dest_url, .. } => {
                self.inline_stack.push(InlineFrame::Link);
                self.active_link = Some(ActiveLink {
                    target: dest_url.to_string(),
                    has_visible_text: false,
                });
            }
            Tag::Image { dest_url, .. } => {
                // Image nodes print only the URL.
                self.push_span(dest_url.to_string(), self.theme.dim);
            }
            Tag::MetadataBlock(_) => {
                // Silently drop YAML/TOML front matter.
                self.open_block();
                self.in_code_block = true;
                self.inline_stack.push(InlineFrame::CodeBlock);
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.close_text_block(),
            TagEnd::Heading(_) => {
                self.heading_level = None;
                self.close_text_block();
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.close_block();
            }
            TagEnd::CodeBlock => {
                if self.code_block_line_has_content || !self.current.is_empty() {
                    self.flush_code_block_line();
                }
                self.in_code_block = false;
                self.code_block_language = None;
                self.code_block_line_has_content = false;
                pop_frame(&mut self.inline_stack, |f| {
                    matches!(f, InlineFrame::CodeBlock)
                });
                self.close_block();
                self.suppress_next_block_separator = true;
            }
            TagEnd::HtmlBlock => self.close_block(),
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.close_block();
            }
            TagEnd::Item => {
                if self.pending_list_marker.is_some() {
                    self.flush_pending_list_marker();
                }
                if !self.current.is_empty() {
                    self.flush_line();
                }
                // Advance the numbering on the active ordered list.
                if let Some(Some(ref mut n)) = self.list_stack.last_mut() {
                    *n += 1;
                }
            }
            TagEnd::FootnoteDefinition => self.close_block(),
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => self.close_block(),
            TagEnd::Table => {
                self.flush_table();
                self.in_table = false;
                self.close_block();
            }
            TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {
                // handled in handle_table_event
            }
            TagEnd::Emphasis => {
                pop_frame(&mut self.inline_stack, |f| {
                    matches!(f, InlineFrame::Emphasis)
                });
            }
            TagEnd::Strong => {
                pop_frame(&mut self.inline_stack, |f| matches!(f, InlineFrame::Strong));
            }
            TagEnd::Strikethrough => {}
            TagEnd::Link => {
                let active = self.active_link.take();
                pop_frame(&mut self.inline_stack, |f| matches!(f, InlineFrame::Link));
                if let Some(active) = active {
                    if !active.has_visible_text && !active.target.is_empty() {
                        let style = merge(self.theme.link, self.current_inline_modifiers());
                        self.append_span(active.target.clone(), style, Some(active.target));
                    }
                }
            }
            TagEnd::Image => {}
            TagEnd::MetadataBlock(_) => {
                if self.code_block_line_has_content || !self.current.is_empty() {
                    self.flush_code_block_line();
                }
                self.in_code_block = false;
                self.code_block_language = None;
                self.code_block_line_has_content = false;
                pop_frame(&mut self.inline_stack, |f| {
                    matches!(f, InlineFrame::CodeBlock)
                });
                self.close_block();
            }
        }
    }

    fn open_block(&mut self) -> bool {
        if let Some(marker) = standalone_ordered_marker(&self.current, self.theme.text) {
            self.current.clear();
            self.current_hyperlinks.clear();
            self.current_formula_ranges.clear();
            self.pending_list_marker = Some(marker);
        }
        if !self.current.is_empty() {
            self.flush_line();
        }
        let follows_previous_block = self.has_emitted_block && self.pending_list_marker.is_none();
        if follows_previous_block {
            if self.suppress_next_block_separator {
                self.suppress_next_block_separator = false;
            } else {
                // Block separator — single blank line between blocks.
                self.lines.push(Line::from(String::new()));
                self.line_hyperlinks.push(Vec::new());
            }
        }
        follows_previous_block
    }

    fn open_tight_block(&mut self) {
        self.open_block();
        self.suppress_next_block_separator = true;
        if matches!(self.lines.last(), Some(line) if line_is_blank(line)) {
            self.lines.pop();
            self.line_hyperlinks.pop();
        }
    }

    fn close_text_block(&mut self) {
        self.close_block();
        self.active_link = None;
        self.inline_stack
            .retain(|frame| matches!(frame, InlineFrame::CodeBlock));
    }

    fn close_block(&mut self) {
        if let Some(marker) = standalone_ordered_marker(&self.current, self.theme.text) {
            self.current.clear();
            self.current_hyperlinks.clear();
            self.current_formula_ranges.clear();
            self.pending_list_marker = Some(marker);
            return;
        }
        if self.pending_list_marker.is_some() {
            self.flush_pending_list_marker();
        }
        if !self.current.is_empty() {
            self.flush_line();
        }
        self.has_emitted_block = true;
    }

    fn begin_list_item(&mut self) {
        if self.pending_list_marker.is_some() {
            self.flush_pending_list_marker();
        }
        if !self.current.is_empty() {
            self.flush_line();
        }
        let depth = self.list_stack.len().saturating_sub(1);
        let indent = "  ".repeat(depth);
        let marker = match self.list_stack.last().copied() {
            Some(Some(n)) => format!("{n}. "),
            Some(None) => "- ".to_string(),
            None => String::new(),
        };
        self.pending_list_marker = Some(Span::styled(format!("{indent}{marker}"), self.theme.text));
    }

    fn flush_pending_list_marker(&mut self) {
        if let Some(marker) = self.pending_list_marker.take() {
            while let Some(line) = self.lines.last() {
                if line_is_blank(line) || line_is_standalone_list_marker(line) {
                    self.lines.pop();
                    self.line_hyperlinks.pop();
                    continue;
                }
                break;
            }
            self.current.push(marker);
        }
    }

    fn push_rule(&mut self) {
        self.open_block();
        self.current
            .push(Span::styled("---".to_string(), self.theme.dim));
        self.flush_line();
        self.has_emitted_block = true;
    }

    fn push_source_text(&mut self, text: &str, source_range: Range<usize>) {
        if !self.render_options.math
            || !self
                .literal_math_ranges
                .iter()
                .any(|range| ranges_overlap(range, &source_range))
        {
            self.push_text(text);
            return;
        }

        if source_range.len() != text.len() {
            self.push_literal_math_text(text);
            return;
        }

        let mut cursor = 0usize;
        let overlapping = self
            .literal_math_ranges
            .iter()
            .filter_map(|range| intersect_ranges(range, &source_range))
            .collect::<Vec<_>>();
        for overlap in overlapping {
            let start = overlap.start.saturating_sub(source_range.start);
            let end = overlap.end.saturating_sub(source_range.start);
            if start > cursor {
                self.push_text(&text[cursor..start]);
            }
            if end > start {
                self.push_literal_math_text(&text[start..end]);
            }
            cursor = end;
        }
        if cursor < text.len() {
            self.push_text(&text[cursor..]);
        }
    }

    fn push_literal_math_text(&mut self, text: &str) {
        let mut first = true;
        for segment in text.split('\n') {
            if !first {
                self.flush_line();
            }
            if !segment.is_empty() {
                let style = merge(self.theme.code, self.current_inline_modifiers());
                self.push_span_with_style(segment.to_string(), style);
            }
            first = false;
        }
    }

    fn push_parsed_formula(
        &mut self,
        parser_expression: &str,
        source_range: Range<usize>,
        source: &str,
        parser_display: FormulaDisplayMode,
    ) {
        let fragment = self
            .math_fragments
            .iter()
            .find(|fragment| fragment.source_range == source_range)
            .cloned();
        if let Some(fragment) = fragment {
            let display = match fragment.display {
                MathDisplayMode::Inline => FormulaDisplayMode::Inline,
                MathDisplayMode::Display => FormulaDisplayMode::Display,
            };
            self.push_formula(
                fragment.expression(source),
                fragment.source(source),
                display,
            );
        } else {
            self.push_formula(parser_expression, &source[source_range], parser_display);
        }
    }

    fn push_formula(&mut self, expression: &str, source: &str, display: FormulaDisplayMode) {
        let prepared = catch_unwind(AssertUnwindSafe(|| {
            prepare_formula(
                expression,
                source,
                display,
                self.terminal_width,
                self.theme.text.fg,
            )
        }))
        .ok()
        .and_then(Result::ok);
        let Some(prepared) = prepared else {
            let style = merge(self.theme.code, self.current_inline_modifiers());
            self.push_span_with_style(source.to_string(), style);
            return;
        };

        if display == FormulaDisplayMode::Inline
            && !self.current.is_empty()
            && current_display_width(&self.current)
                .saturating_add(prepared.terminal_columns as usize)
                > self.terminal_width
        {
            self.flush_line();
        }
        if display == FormulaDisplayMode::Display && !self.current.is_empty() {
            self.flush_line();
        }

        let formula = self.formulas.len();
        self.formulas.push(RenderedFormula {
            source: source.to_string(),
            expression: expression.to_string(),
            display,
            ranges: Vec::new(),
            terminal_columns: prepared.terminal_columns,
            terminal_rows: prepared.terminal_rows,
            fallback: prepared.fallback.clone(),
            asset: prepared.asset,
        });

        for (row, line) in prepared.fallback.lines.into_iter().enumerate() {
            if display == FormulaDisplayMode::Display {
                let left = self
                    .terminal_width
                    .saturating_sub(prepared.terminal_columns as usize)
                    / 2;
                if left > 0 {
                    self.push_span(" ".repeat(left), self.theme.text);
                }
            }
            self.append_formula_line(formula, line);
            if display == FormulaDisplayMode::Display || row + 1 < prepared.terminal_rows as usize {
                self.flush_line();
            }
        }
    }

    fn append_formula_line(&mut self, formula: usize, line: Line<'static>) {
        self.flush_pending_list_marker();
        let start_byte = spans_text_len(&self.current);
        for span in line.spans {
            self.current.push(span);
        }
        let end_byte = spans_text_len(&self.current);
        if end_byte <= start_byte {
            return;
        }
        self.current_formula_ranges.push(CurrentFormulaRange {
            formula,
            start_byte,
            end_byte,
        });
        if let Some(target) = self.active_link.as_mut().map(|active| {
            active.has_visible_text = true;
            active.target.clone()
        }) {
            self.record_current_hyperlink(start_byte, end_byte, target);
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some(marker) = standalone_ordered_marker(&self.current, self.theme.text) {
            self.current.clear();
            self.current_hyperlinks.clear();
            self.current_formula_ranges.clear();
            self.pending_list_marker = Some(marker);
        } else {
            let previous_lines = self.lines.len();
            if let Some(marker) =
                pending_marker_from_emitted_loose_item(&mut self.lines, self.theme.text)
            {
                self.line_hyperlinks.truncate(self.lines.len());
                self.pending_list_marker = Some(marker);
            } else {
                debug_assert_eq!(previous_lines, self.lines.len());
            }
        }
        if self.in_code_block {
            self.flush_pending_list_marker();
            self.push_code_block_text(text);
            return;
        }
        // A block of `Text` may contain embedded newlines (e.g. from a
        // HTML block event). Split so each physical line becomes a
        // separate `Line` in the output.
        let mut first = true;
        for segment in text.split('\n') {
            if !first {
                self.flush_line();
            }
            if !segment.is_empty() {
                self.flush_pending_list_marker();
                if self.active_link.is_some() {
                    let style = self.current_inline_style();
                    self.push_span_with_style(segment.to_string(), style);
                } else {
                    self.push_text_with_bare_urls(segment);
                }
            }
            first = false;
        }
    }

    fn push_text_with_bare_urls(&mut self, text: &str) {
        let ranges = find_bare_url_ranges(text);
        if ranges.is_empty() {
            self.push_span_with_style(text.to_string(), self.current_inline_style());
            return;
        }

        let mut cursor = 0;
        for (start, end) in ranges {
            if start > cursor {
                self.push_span_with_style(
                    text[cursor..start].to_string(),
                    self.current_inline_style(),
                );
            }
            let target = text[start..end].to_string();
            let style = merge(self.theme.link, self.current_inline_modifiers());
            self.append_span(target.clone(), style, Some(target));
            cursor = end;
        }
        if cursor < text.len() {
            self.push_span_with_style(text[cursor..].to_string(), self.current_inline_style());
        }
    }

    fn push_code_block_text(&mut self, text: &str) {
        let mut first = true;
        for segment in text.split('\n') {
            if !first {
                self.flush_code_block_line();
            }
            if !segment.is_empty() {
                self.push_code_highlighted_segment(segment);
            }
            first = false;
        }
    }

    fn push_code_highlighted_segment(&mut self, segment: &str) {
        let language = self.code_block_language.as_deref();
        let spans = highlight_code_line(segment, language, &self.theme);
        self.code_block_line_has_content |= !segment.is_empty();
        self.current.extend(spans);
    }

    fn flush_code_block_line(&mut self) {
        if !self.code_block_line_has_content && self.current.is_empty() {
            self.lines.push(Line::from(Span::styled(
                " ".to_string(),
                self.theme.code_block,
            )));
            self.line_hyperlinks.push(Vec::new());
            return;
        }
        if !self.current.is_empty() {
            self.flush_line();
        }
        self.code_block_line_has_content = false;
    }

    fn push_span(&mut self, content: String, base: Style) {
        let base = if self.active_link.is_some() {
            self.theme.link
        } else {
            base
        };
        let style = merge(base, self.current_inline_modifiers());
        self.push_span_with_style(content, style);
    }

    fn push_span_with_style(&mut self, content: String, style: Style) {
        let hyperlink_target = self.active_link.as_ref().map(|link| link.target.clone());
        let has_content = !content.is_empty();
        self.append_span(content, style, hyperlink_target);
        if has_content {
            if let Some(link) = self.active_link.as_mut() {
                link.has_visible_text = true;
            }
        }
    }

    fn append_span(&mut self, content: String, style: Style, hyperlink_target: Option<String>) {
        self.flush_pending_list_marker();
        let start_byte = spans_text_len(&self.current);
        let end_byte = start_byte.saturating_add(content.len());
        self.current.push(Span::styled(content, style));
        if let Some(target) = hyperlink_target.filter(|_| end_byte > start_byte) {
            self.record_current_hyperlink(start_byte, end_byte, target);
        }
    }

    fn record_current_hyperlink(&mut self, start_byte: usize, end_byte: usize, target: String) {
        if let Some(last) = self.current_hyperlinks.last_mut() {
            if last.end_byte == start_byte && last.target == target {
                last.end_byte = end_byte;
                return;
            }
        }
        self.current_hyperlinks.push(CurrentHyperlinkRange {
            start_byte,
            end_byte,
            target,
        });
    }

    fn current_inline_style(&self) -> Style {
        let base = if self.active_link.is_some() {
            self.theme.link
        } else if let Some(level) = self.heading_level {
            match level {
                HeadingLevel::H1 => self.theme.heading,
                _ => self.theme.heading_h2,
            }
        } else {
            self.theme.text
        };
        merge(base, self.current_inline_modifiers())
    }

    fn current_inline_modifiers(&self) -> Modifier {
        let mut m = Modifier::empty();
        for frame in &self.inline_stack {
            match frame {
                InlineFrame::Emphasis => m |= Modifier::ITALIC,
                InlineFrame::Strong => m |= Modifier::BOLD,
                InlineFrame::Link | InlineFrame::CodeBlock => {}
            }
        }
        m
    }

    fn flush_line(&mut self) {
        let mut spans = std::mem::take(&mut self.current);
        let line = self.lines.len();
        let prefix_bytes = if self.blockquote_depth > 0 {
            // Prepend the blockquote bar once per depth.
            let prefix = BLOCKQUOTE_BAR.repeat(self.blockquote_depth);
            let prefix_bytes = prefix.len();
            let bar = Span::styled(prefix, self.theme.blockquote_bar);
            spans.insert(0, bar);
            prefix_bytes
        } else {
            0
        };
        let ranges = std::mem::take(&mut self.current_hyperlinks)
            .into_iter()
            .map(|range| HyperlinkRange {
                line,
                start_byte: prefix_bytes.saturating_add(range.start_byte),
                end_byte: prefix_bytes.saturating_add(range.end_byte),
                target: range.target,
            })
            .collect();
        for range in std::mem::take(&mut self.current_formula_ranges) {
            if let Some(formula) = self.formulas.get_mut(range.formula) {
                formula.ranges.push(FormulaRange {
                    line,
                    start_byte: prefix_bytes.saturating_add(range.start_byte),
                    end_byte: prefix_bytes.saturating_add(range.end_byte),
                });
            }
        }
        self.lines.push(Line::from(spans));
        self.line_hyperlinks.push(ranges);
    }

    fn handle_table_event(&mut self, event: &Event<'_>, raw_source: &str) -> bool {
        match event {
            Event::Start(tag) => match tag {
                Tag::TableHead | Tag::TableRow => {
                    self.table_current_row = Some(Vec::new());
                    true
                }
                Tag::TableCell => {
                    self.table_current_cell = Some(TableCell::default());
                    self.table_cell_buf = Some(String::new());
                    self.table_literal_math = false;
                    true
                }
                Tag::Emphasis => {
                    if let Some(cell) = self.table_current_cell.as_mut() {
                        cell.modifiers |= Modifier::ITALIC;
                        true
                    } else {
                        false
                    }
                }
                Tag::Strong => {
                    if let Some(cell) = self.table_current_cell.as_mut() {
                        cell.modifiers |= Modifier::BOLD;
                        true
                    } else {
                        false
                    }
                }
                Tag::Link { dest_url, .. } => {
                    if let Some(cell) = self.table_current_cell.as_mut() {
                        cell.pending_link_dest = Some(dest_url.to_string());
                        cell.pending_link_start = cell.plain.len();
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            },
            Event::End(tag) => match tag {
                TagEnd::TableCell => {
                    let cell = self.table_current_cell.take().unwrap_or_default();
                    self.table_cell_buf = None;
                    self.table_literal_math = false;
                    if let Some(row) = self.table_current_row.as_mut() {
                        row.push(cell);
                    }
                    true
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(row) = self.table_current_row.take() {
                        self.table_rows.push(row);
                    }
                    true
                }
                TagEnd::Emphasis => {
                    if let Some(cell) = self.table_current_cell.as_mut() {
                        cell.modifiers.remove(Modifier::ITALIC);
                        true
                    } else {
                        false
                    }
                }
                TagEnd::Strong => {
                    if let Some(cell) = self.table_current_cell.as_mut() {
                        cell.modifiers.remove(Modifier::BOLD);
                        true
                    } else {
                        false
                    }
                }
                TagEnd::Link => {
                    if let Some(cell) = self.table_current_cell.as_mut() {
                        if let Some(dest) = cell.pending_link_dest.take() {
                            if cell.plain.len() == cell.pending_link_start && !dest.is_empty() {
                                cell.push_text(&dest);
                            }
                            let end_byte = cell.plain.len();
                            if end_byte > cell.pending_link_start {
                                cell.annotations.push(TableCellAnnotation {
                                    start_byte: cell.pending_link_start,
                                    end_byte,
                                    style: merge(self.theme.link, cell.modifiers),
                                    target: Some(dest),
                                });
                            }
                        }
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            },
            Event::Text(t) => {
                if self.render_options.math && t.contains('$') {
                    self.table_literal_math = true;
                }
                if let Some(cell) = self.table_current_cell.as_mut() {
                    if self.table_literal_math {
                        cell.push_code(t, self.theme.code);
                        return true;
                    }
                    let base = cell.plain.len();
                    cell.push_text(t);
                    if cell.pending_link_dest.is_none() {
                        for (start, end) in find_bare_url_ranges(t.as_ref()) {
                            let target = t[start..end].to_string();
                            cell.annotations.push(TableCellAnnotation {
                                start_byte: base.saturating_add(start),
                                end_byte: base.saturating_add(end),
                                style: merge(self.theme.link, cell.modifiers),
                                target: Some(target),
                            });
                        }
                    }
                    true
                } else {
                    false
                }
            }
            Event::Code(c) => {
                if let Some(cell) = self.table_current_cell.as_mut() {
                    cell.push_code(c, self.theme.code);
                    true
                } else {
                    false
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(cell) = self.table_current_cell.as_mut() {
                    if self.table_literal_math {
                        cell.push_code(" ", self.theme.code);
                    } else {
                        cell.push_text(" ");
                    }
                    true
                } else {
                    false
                }
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => {
                if let Some(cell) = self.table_current_cell.as_mut() {
                    // Table cells have their own wrapping/layout pipeline and cannot
                    // safely host a rectangular bitmap. Preserve the exact formula
                    // source as the documented unsupported-context fallback.
                    cell.push_code(raw_source, self.theme.code);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn flush_table(&mut self) {
        if let Some(cell) = self.table_current_cell.take() {
            if let Some(row) = self.table_current_row.as_mut() {
                row.push(cell);
            }
        }
        if let Some(row) = self.table_current_row.take() {
            self.table_rows.push(row);
        }
        self.table_cell_buf = None;
        let rows = std::mem::take(&mut self.table_rows)
            .into_iter()
            .filter(|row| !row.is_empty())
            .collect::<Vec<_>>();
        let alignments = std::mem::take(&mut self.table_align);
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let header = normalize_table_row(rows.first().cloned().unwrap_or_default(), cols);
        let body_rows = rows
            .into_iter()
            .skip(1)
            .map(|row| normalize_table_row(row, cols))
            .collect::<Vec<_>>();
        if body_rows.is_empty() {
            return;
        }
        let align = (0..cols)
            .map(|idx| cmark_to_table_alignment(alignments.get(idx).copied()))
            .collect::<Vec<_>>();
        let available_width = compute_available_width(cols, self.terminal_width);
        let min_widths = table_min_widths(&header, &body_rows);
        let ideal_widths = table_ideal_widths(&header, &body_rows);
        let layout = compute_column_widths(&min_widths, &ideal_widths, available_width);
        let mut table_annotations = Vec::new();
        let wrapped_header = header
            .iter()
            .zip(layout.widths.iter())
            .map(|(cell, width)| CellLines {
                lines: wrap_marked_table_cell(
                    cell,
                    &mut table_annotations,
                    *width,
                    layout.needs_hard_wrap,
                ),
            })
            .collect::<Vec<_>>();
        let wrapped_rows = body_rows
            .iter()
            .map(|row| {
                row.iter()
                    .zip(layout.widths.iter())
                    .map(|(cell, width)| CellLines {
                        lines: wrap_marked_table_cell(
                            cell,
                            &mut table_annotations,
                            *width,
                            layout.needs_hard_wrap,
                        ),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let table = TableInput::new(wrapped_header, wrapped_rows, align, layout);
        let rendered = render_horizontal_table(&table, self.terminal_width);
        debug_assert!(
            rendered
                .lines
                .iter()
                .all(|line| table_markers_balance(line)),
            "unbalanced table markers: {:?}",
            rendered.lines
        );
        let style = self.theme.text;
        let mut active_annotations = Vec::new();
        let mut ansi_bold = false;
        for line in rendered.lines {
            let line_index = self.lines.len();
            let (line, hyperlinks) = render_marked_table_line(
                &line,
                style,
                &table_annotations,
                &mut active_annotations,
                &mut ansi_bold,
                line_index,
            );
            self.lines.push(line);
            self.line_hyperlinks.push(hyperlinks);
        }
        self.has_emitted_block = true;
        self.suppress_next_block_separator = true;
    }
}

#[derive(Debug, Clone, Default)]
struct TableCell {
    plain: String,
    modifiers: Modifier,
    annotations: Vec<TableCellAnnotation>,
    pending_link_dest: Option<String>,
    pending_link_start: usize,
}

#[derive(Debug, Clone)]
struct TableCellAnnotation {
    start_byte: usize,
    end_byte: usize,
    style: Style,
    target: Option<String>,
}

#[derive(Debug, Clone)]
struct TableMarkerAnnotation {
    style: Style,
    target: Option<String>,
}

impl TableCell {
    fn push_text(&mut self, text: &str) {
        self.plain.push_str(text);
    }

    fn push_code(&mut self, code: &str, style: Style) {
        let start_byte = self.plain.len();
        self.plain.push_str(code);
        let end_byte = self.plain.len();
        if end_byte > start_byte {
            self.annotations.push(TableCellAnnotation {
                start_byte,
                end_byte,
                style,
                target: None,
            });
        }
    }
}

const TABLE_MARKER_BOUNDARY: char = '\u{2060}';
const TABLE_MARKER_START: char = '\u{200b}';
const TABLE_MARKER_END: char = '\u{200c}';
const TABLE_MARKER_ZERO: char = '\u{fe00}';
const TABLE_MARKER_ONE: char = '\u{fe01}';

fn wrap_marked_table_cell(
    cell: &TableCell,
    registry: &mut Vec<TableMarkerAnnotation>,
    width: usize,
    hard: bool,
) -> Vec<String> {
    let marked = mark_table_cell(cell, registry);
    let mut lines = wrap_table_cell(&marked, width, hard);
    let mut active = Vec::new();
    for line in &mut lines {
        let active_at_start = active.clone();
        let mut cursor = 0usize;
        while cursor < line.len() {
            let rest = &line[cursor..];
            if let Some((is_start, id, consumed)) = decode_table_marker(rest) {
                if is_start {
                    active.push(id);
                } else if let Some(position) = active.iter().rposition(|active_id| *active_id == id)
                {
                    active.remove(position);
                }
                cursor = cursor.saturating_add(consumed);
            } else {
                cursor = cursor.saturating_add(
                    rest.chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(rest.len()),
                );
            }
        }

        if !active_at_start.is_empty() {
            let mut prefix = String::new();
            for id in active_at_start {
                push_table_marker(&mut prefix, true, id);
            }
            line.insert_str(0, &prefix);
        }
        if !active.is_empty() {
            for id in active.iter().rev().copied() {
                push_table_marker(line, false, id);
            }
        }
    }
    lines
}

fn mark_table_cell(cell: &TableCell, registry: &mut Vec<TableMarkerAnnotation>) -> String {
    let mut events = Vec::new();
    for annotation in &cell.annotations {
        if annotation.start_byte >= annotation.end_byte
            || annotation.end_byte > cell.plain.len()
            || !cell.plain.is_char_boundary(annotation.start_byte)
            || !cell.plain.is_char_boundary(annotation.end_byte)
        {
            continue;
        }
        let id = registry.len();
        registry.push(TableMarkerAnnotation {
            style: annotation.style,
            target: annotation.target.clone(),
        });
        events.push((annotation.start_byte, true, id));
        events.push((annotation.end_byte, false, id));
    }
    events.sort_unstable_by_key(|(position, is_start, _)| (*position, *is_start));

    let mut marked = String::with_capacity(cell.plain.len().saturating_add(events.len() * 4));
    let mut cursor = 0;
    for (position, is_start, id) in events {
        if position > cursor {
            marked.push_str(&cell.plain[cursor..position]);
        }
        push_table_marker(&mut marked, is_start, id);
        cursor = position;
    }
    marked.push_str(&cell.plain[cursor..]);
    marked
}

fn push_table_marker(output: &mut String, is_start: bool, id: usize) {
    output.push(TABLE_MARKER_BOUNDARY);
    output.push(if is_start {
        TABLE_MARKER_START
    } else {
        TABLE_MARKER_END
    });
    let value = id.saturating_add(1);
    let bits = usize::BITS.saturating_sub(value.leading_zeros());
    for shift in (0..bits).rev() {
        output.push(if value & (1usize << shift) == 0 {
            TABLE_MARKER_ZERO
        } else {
            TABLE_MARKER_ONE
        });
    }
    output.push(TABLE_MARKER_BOUNDARY);
}

fn decode_table_marker(text: &str) -> Option<(bool, usize, usize)> {
    let mut chars = text.char_indices();
    let (_, boundary) = chars.next()?;
    if boundary != TABLE_MARKER_BOUNDARY {
        return None;
    }
    let (_, kind) = chars.next()?;
    let is_start = match kind {
        TABLE_MARKER_START => true,
        TABLE_MARKER_END => false,
        _ => return None,
    };
    let mut value = 0usize;
    let mut has_bit = false;
    for (offset, ch) in chars {
        if ch == TABLE_MARKER_BOUNDARY {
            return has_bit
                .then(|| value.checked_sub(1))
                .flatten()
                .map(|id| (is_start, id, offset + ch.len_utf8()));
        }
        let bit = match ch {
            TABLE_MARKER_ZERO => 0,
            TABLE_MARKER_ONE => 1,
            _ => return None,
        };
        value = value.checked_mul(2)?.checked_add(bit)?;
        has_bit = true;
    }
    None
}

fn table_markers_balance(marked: &str) -> bool {
    let mut active = Vec::new();
    let mut cursor = 0usize;
    while cursor < marked.len() {
        let rest = &marked[cursor..];
        if let Some((is_start, id, consumed)) = decode_table_marker(rest) {
            if is_start {
                active.push(id);
            } else if let Some(position) = active.iter().rposition(|active_id| *active_id == id) {
                active.remove(position);
            } else {
                return false;
            }
            cursor = cursor.saturating_add(consumed);
        } else {
            cursor = cursor.saturating_add(
                rest.chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(rest.len()),
            );
        }
    }
    active.is_empty()
}

fn render_marked_table_line(
    marked: &str,
    base_style: Style,
    registry: &[TableMarkerAnnotation],
    active: &mut Vec<usize>,
    ansi_bold: &mut bool,
    line_index: usize,
) -> (Line<'static>, Vec<HyperlinkRange>) {
    const ANSI_BOLD_START: &str = "\x1b[1m";
    const ANSI_BOLD_END: &str = "\x1b[22m";

    let mut spans = Vec::new();
    let mut hyperlinks: Vec<HyperlinkRange> = Vec::new();
    let mut visible_bytes = 0usize;
    let mut cursor = 0usize;
    while cursor < marked.len() {
        let rest = &marked[cursor..];
        if rest.starts_with(ANSI_BOLD_START) {
            *ansi_bold = true;
            cursor = cursor.saturating_add(ANSI_BOLD_START.len());
            continue;
        }
        if rest.starts_with(ANSI_BOLD_END) {
            *ansi_bold = false;
            cursor = cursor.saturating_add(ANSI_BOLD_END.len());
            continue;
        }
        if let Some((is_start, id, consumed)) = decode_table_marker(rest) {
            if is_start {
                active.push(id);
            } else if let Some(position) = active.iter().rposition(|active_id| *active_id == id) {
                active.remove(position);
            }
            cursor = cursor.saturating_add(consumed);
            continue;
        }

        let next = rest
            .char_indices()
            .skip(1)
            .find_map(|(offset, ch)| {
                (ch == TABLE_MARKER_BOUNDARY || ch == '\x1b').then_some(offset)
            })
            .unwrap_or(rest.len());
        let segment = &rest[..next];
        let mut style = base_style;
        if *ansi_bold {
            style = merge(style, Modifier::BOLD);
        }
        for id in active.iter().copied() {
            if let Some(annotation) = registry.get(id) {
                style = style.patch(annotation.style);
            }
        }
        spans.push(Span::styled(segment.to_string(), style));

        if let Some(target) = active
            .iter()
            .rev()
            .filter_map(|id| registry.get(*id))
            .find_map(|annotation| annotation.target.as_ref())
        {
            let end_byte = visible_bytes.saturating_add(segment.len());
            if let Some(last) = hyperlinks.last_mut() {
                if last.end_byte == visible_bytes && last.target == *target {
                    last.end_byte = end_byte;
                } else {
                    hyperlinks.push(HyperlinkRange {
                        line: line_index,
                        start_byte: visible_bytes,
                        end_byte,
                        target: target.clone(),
                    });
                }
            } else {
                hyperlinks.push(HyperlinkRange {
                    line: line_index,
                    start_byte: visible_bytes,
                    end_byte,
                    target: target.clone(),
                });
            }
        }
        visible_bytes = visible_bytes.saturating_add(segment.len());
        cursor = cursor.saturating_add(next);
    }
    (Line::from(spans), hyperlinks)
}

fn normalize_table_row(mut row: Vec<TableCell>, cols: usize) -> Vec<TableCell> {
    row.resize_with(cols, TableCell::default);
    row
}

fn cmark_to_table_alignment(alignment: Option<CmarkAlignment>) -> TableAlignment {
    match alignment.unwrap_or(CmarkAlignment::None) {
        CmarkAlignment::None | CmarkAlignment::Left => TableAlignment::Left,
        CmarkAlignment::Center => TableAlignment::Center,
        CmarkAlignment::Right => TableAlignment::Right,
    }
}

fn table_min_widths(header: &[TableCell], rows: &[Vec<TableCell>]) -> Vec<usize> {
    table_column_cells(header, rows)
        .map(|cells| {
            cells
                .map(|cell| {
                    cell.plain
                        .split_whitespace()
                        .map(display_width)
                        .max()
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
                .max(MIN_COLUMN_WIDTH)
        })
        .collect()
}

fn table_ideal_widths(header: &[TableCell], rows: &[Vec<TableCell>]) -> Vec<usize> {
    table_column_cells(header, rows)
        .map(|cells| {
            cells
                .map(|cell| display_width(&cell.plain))
                .max()
                .unwrap_or(0)
                .max(MIN_COLUMN_WIDTH)
        })
        .collect()
}

fn table_column_cells<'a>(
    header: &'a [TableCell],
    rows: &'a [Vec<TableCell>],
) -> impl Iterator<Item = impl Iterator<Item = &'a TableCell>> + 'a {
    (0..header.len()).map(move |idx| {
        std::iter::once(&header[idx]).chain(rows.iter().filter_map(move |row| row.get(idx)))
    })
}

fn wrap_table_cell(text: &str, width: usize, hard: bool) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for physical in text.split('\n') {
        if physical.is_empty() {
            out.push(String::new());
            continue;
        }
        if hard {
            wrap_hard(physical, width, &mut out);
        } else {
            wrap_words(physical, width, &mut out);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn wrap_words(text: &str, width: usize, out: &mut Vec<String>) {
    let mut line = String::new();
    let mut line_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = display_width(word);
        if line.is_empty() {
            if word_width > width {
                wrap_hard(word, width, out);
            } else {
                line.push_str(word);
                line_width = word_width;
            }
            continue;
        }
        if line_width + 1 + word_width <= width {
            line.push(' ');
            line.push_str(word);
            line_width += 1 + word_width;
        } else {
            out.push(std::mem::take(&mut line));
            line_width = 0;
            if word_width > width {
                wrap_hard(word, width, out);
            } else {
                line.push_str(word);
                line_width = word_width;
            }
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
}

fn wrap_hard(text: &str, width: usize, out: &mut Vec<String>) {
    let mut line = String::new();
    let mut line_width = 0usize;
    for grapheme in text.graphemes(true) {
        let w = display_width(grapheme);
        if line_width > 0 && line_width + w > width {
            out.push(std::mem::take(&mut line));
            line_width = 0;
        }
        line.push_str(grapheme);
        line_width += w;
    }
    if !line.is_empty() {
        out.push(line);
    }
}

#[derive(Debug, Clone)]
struct PreparedFormula {
    terminal_columns: u16,
    terminal_rows: u16,
    fallback: Text<'static>,
    asset: FormulaAsset,
}

fn prepare_formula(
    expression: &str,
    source: &str,
    display: FormulaDisplayMode,
    terminal_width: usize,
    foreground: Option<Color>,
) -> Result<PreparedFormula, FormulaRenderError> {
    let (red, green, blue) = terminal_rgb(foreground.unwrap_or(Color::White));
    let rendered = render_formula(
        expression,
        source,
        FormulaRenderOptions {
            display: match display {
                FormulaDisplayMode::Inline => CoreFormulaDisplayMode::Inline,
                FormulaDisplayMode::Display => CoreFormulaDisplayMode::Display,
            },
            max_width_cells: terminal_width,
            // Terminal cells: keep the legacy one-row inline clamp.
            max_height_cells: None,
            foreground: FormulaColor::new(red, green, blue),
        },
    )?;
    let fallback = bitmap_to_half_blocks(&rendered.asset.bitmap);
    Ok(PreparedFormula {
        terminal_columns: rendered.terminal_columns,
        terminal_rows: rendered.terminal_rows,
        fallback,
        asset: rendered.asset,
    })
}

fn terminal_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Reset | Color::White => (255, 255, 255),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (204, 204, 204),
        Color::DarkGray => (118, 118, 118),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(index) => indexed_rgb(index),
    }
}

fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let cube = index - 16;
            let component = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
            (
                component(cube / 36),
                component((cube % 36) / 6),
                component(cube % 6),
            )
        }
        _ => {
            let gray = 8 + (index - 232) * 10;
            (gray, gray, gray)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SampledColor {
    red: u8,
    green: u8,
    blue: u8,
}

fn bitmap_to_half_blocks(bitmap: &FormulaBitmap) -> Text<'static> {
    let columns = bitmap.width.div_ceil(FORMULA_CELL_WIDTH_PX);
    let rows = bitmap.height.div_ceil(FORMULA_CELL_HEIGHT_PX);
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for column in 0..columns {
            let x0 = column * FORMULA_CELL_WIDTH_PX;
            let x1 = (x0 + FORMULA_CELL_WIDTH_PX).min(bitmap.width);
            let y0 = row * FORMULA_CELL_HEIGHT_PX;
            let middle = (y0 + FORMULA_CELL_HEIGHT_PX / 2).min(bitmap.height);
            let y1 = (y0 + FORMULA_CELL_HEIGHT_PX).min(bitmap.height);
            let top = sample_bitmap(bitmap, x0, x1, y0, middle);
            let bottom = sample_bitmap(bitmap, x0, x1, middle, y1);
            let (glyph, style) = half_block_cell(top, bottom);
            if let Some(last) = spans.last_mut() {
                if last.style == style {
                    last.content.to_mut().push(glyph);
                    continue;
                }
            }
            spans.push(Span::styled(glyph.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn sample_bitmap(
    bitmap: &FormulaBitmap,
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
) -> Option<SampledColor> {
    let mut alpha = 0u64;
    let mut red = 0u64;
    let mut green = 0u64;
    let mut blue = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let index = (u64::from(y) * u64::from(bitmap.width) + u64::from(x)) * 4;
            let index = index as usize;
            let pixel_alpha = u64::from(bitmap.rgba.get(index + 3).copied().unwrap_or(0));
            alpha += pixel_alpha;
            red += u64::from(bitmap.rgba.get(index).copied().unwrap_or(0)) * pixel_alpha;
            green += u64::from(bitmap.rgba.get(index + 1).copied().unwrap_or(0)) * pixel_alpha;
            blue += u64::from(bitmap.rgba.get(index + 2).copied().unwrap_or(0)) * pixel_alpha;
        }
    }
    let alpha = std::num::NonZeroU64::new(alpha)?;
    Some(SampledColor {
        red: (red / alpha.get()).min(255) as u8,
        green: (green / alpha.get()).min(255) as u8,
        blue: (blue / alpha.get()).min(255) as u8,
    })
}

fn half_block_cell(top: Option<SampledColor>, bottom: Option<SampledColor>) -> (char, Style) {
    match (top, bottom) {
        (None, None) => (' ', Style::new()),
        (Some(top), None) => (
            '▀',
            Style::new().fg(Color::Rgb(top.red, top.green, top.blue)),
        ),
        (None, Some(bottom)) => (
            '▄',
            Style::new().fg(Color::Rgb(bottom.red, bottom.green, bottom.blue)),
        ),
        (Some(top), Some(bottom)) => (
            '▀',
            Style::new()
                .fg(Color::Rgb(top.red, top.green, top.blue))
                .bg(Color::Rgb(bottom.red, bottom.green, bottom.blue)),
        ),
    }
}

fn find_literal_math_ranges<'a>(
    source: &str,
    events: &[(Event<'a>, Range<usize>)],
) -> Vec<Range<usize>> {
    let blocks = events
        .iter()
        .filter_map(|(event, range)| match event {
            Event::Start(Tag::Paragraph | Tag::Heading { .. }) => Some(range.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let protected = events
        .iter()
        .filter_map(|(event, range)| match event {
            Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::Code(_)
            | Event::InlineHtml(_)
            | Event::Html(_)
            | Event::Start(Tag::Link { .. } | Tag::Image { .. }) => Some(range.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();

    let bytes = source.as_bytes();
    blocks
        .into_iter()
        .filter_map(|block| {
            if block.start >= block.end || block.end > source.len() {
                return None;
            }
            (block.start..block.end)
                .find(|index| {
                    bytes[*index] == b'$'
                        && !is_backslash_escaped(bytes, *index)
                        && !protected.iter().any(|range| range.contains(index))
                })
                .map(|start| start..block.end)
        })
        .collect()
}

fn event_is_inline_content(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::Start(
                Tag::Emphasis
                    | Tag::Strong
                    | Tag::Strikethrough
                    | Tag::Link { .. }
                    | Tag::Image { .. }
            )
    )
}

fn is_backslash_escaped(bytes: &[u8], index: usize) -> bool {
    let mut slash_count = 0usize;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        slash_count += 1;
        cursor -= 1;
    }
    slash_count % 2 == 1
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn intersect_ranges(left: &Range<usize>, right: &Range<usize>) -> Option<Range<usize>> {
    let start = left.start.max(right.start);
    let end = left.end.min(right.end);
    (start < end).then_some(start..end)
}

fn current_display_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn display_width(text: &str) -> usize {
    WidthStr::width(text)
}

fn highlight_code_line(
    line: &str,
    language: Option<&str>,
    theme: &MarkdownTheme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let chars = line.char_indices().collect::<Vec<_>>();
    let mut idx = 0usize;
    while idx < chars.len() {
        let (byte_idx, ch) = chars[idx];
        if let Some(comment) = comment_prefix_at(line, byte_idx, language) {
            let _ = comment;
            push_code_span(
                &mut spans,
                &line[byte_idx..],
                theme.code_block.fg(Color::Indexed(244)),
            );
            break;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            let end = quoted_end(line, byte_idx, ch);
            push_code_span(
                &mut spans,
                &line[byte_idx..end],
                theme.code_block.fg(Color::Indexed(114)),
            );
            idx = char_index_at_or_after(&chars, end);
            continue;
        }
        if ch.is_ascii_digit() {
            let end = scan_while(line, byte_idx, |c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_')
            });
            push_code_span(
                &mut spans,
                &line[byte_idx..end],
                theme.code_block.fg(Color::Indexed(179)),
            );
            idx = char_index_at_or_after(&chars, end);
            continue;
        }
        if is_ident_start(ch) {
            let end = scan_while(line, byte_idx, is_ident_continue);
            let token = &line[byte_idx..end];
            let style = if is_keyword(token, language) {
                theme
                    .code_block
                    .fg(Color::Indexed(111))
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.code_block
            };
            push_code_span(&mut spans, token, style);
            idx = char_index_at_or_after(&chars, end);
            continue;
        }
        let end = byte_idx + ch.len_utf8();
        push_code_span(&mut spans, &line[byte_idx..end], theme.code_block);
        idx += 1;
    }
    spans
}

fn push_code_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(text);
            return;
        }
    }
    spans.push(Span::styled(text.to_string(), style));
}

fn comment_prefix_at(line: &str, byte_idx: usize, language: Option<&str>) -> Option<&'static str> {
    let rest = &line[byte_idx..];
    let slash_comment = rest.starts_with("//");
    let hash_comment = rest.starts_with('#');
    match normalized_language(language) {
        Some("py" | "bash" | "sh" | "shell" | "zsh" | "yaml" | "yml" | "toml") => {
            hash_comment.then_some("#")
        }
        Some("json") => None,
        _ => slash_comment
            .then_some("//")
            .or_else(|| hash_comment.then_some("#")),
    }
}

fn quoted_end(line: &str, start: usize, quote: char) -> usize {
    let mut escaped = false;
    for (idx, ch) in line[start + quote.len_utf8()..].char_indices() {
        let absolute = start + quote.len_utf8() + idx;
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return absolute + ch.len_utf8();
        }
    }
    line.len()
}

fn scan_while(line: &str, start: usize, pred: impl Fn(char) -> bool) -> usize {
    for (idx, ch) in line[start..].char_indices() {
        if !pred(ch) {
            return start + idx;
        }
    }
    line.len()
}

fn char_index_at_or_after(chars: &[(usize, char)], byte_idx: usize) -> usize {
    chars
        .iter()
        .position(|(idx, _)| *idx >= byte_idx)
        .unwrap_or(chars.len())
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn is_keyword(token: &str, language: Option<&str>) -> bool {
    const COMMON: &[&str] = &[
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "def",
        "else",
        "enum",
        "export",
        "false",
        "fn",
        "for",
        "from",
        "function",
        "if",
        "impl",
        "import",
        "in",
        "interface",
        "let",
        "loop",
        "match",
        "mod",
        "mut",
        "null",
        "pub",
        "return",
        "self",
        "static",
        "struct",
        "throw",
        "trait",
        "true",
        "try",
        "type",
        "use",
        "var",
        "while",
    ];
    const JSON: &[&str] = &["false", "null", "true"];
    const SHELL: &[&str] = &[
        "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in", "then",
        "until", "while",
    ];
    match normalized_language(language) {
        Some("json") => JSON.contains(&token),
        Some("bash" | "sh" | "shell" | "zsh") => SHELL.contains(&token),
        _ => COMMON.contains(&token),
    }
}

fn normalized_language(language: Option<&str>) -> Option<&str> {
    language.map(str::trim).filter(|s| !s.is_empty()).map(|s| {
        if s.eq_ignore_ascii_case("javascript") {
            "js"
        } else if s.eq_ignore_ascii_case("original") {
            "ts"
        } else if s.eq_ignore_ascii_case("rust") {
            "rs"
        } else if s.eq_ignore_ascii_case("python") {
            "py"
        } else {
            s
        }
    })
}

/// Merge `base` with additional `modifiers` without clobbering the
/// foreground color.
fn merge(base: Style, modifiers: Modifier) -> Style {
    if modifiers.is_empty() {
        base
    } else {
        base.add_modifier(modifiers)
    }
}

fn find_bare_url_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut search_from = 0;

    while search_from < text.len() {
        let Some(start) = next_http_scheme(text, search_from) else {
            break;
        };
        let scheme_len = if text[start..].starts_with("https://") {
            "https://".len()
        } else {
            "http://".len()
        };
        if !is_bare_url_start_boundary(text, start) {
            search_from = start.saturating_add(1);
            continue;
        }

        let candidate_end = text[start..]
            .char_indices()
            .find_map(|(offset, ch)| {
                (offset >= scheme_len && is_bare_url_terminator(ch)).then_some(start + offset)
            })
            .unwrap_or(text.len());
        let end = trim_bare_url_end(text, start, candidate_end);
        if end > start.saturating_add(scheme_len) {
            ranges.push((start, end));
        }
        search_from = candidate_end.max(start.saturating_add(scheme_len));
    }

    ranges
}

fn next_http_scheme(text: &str, from: usize) -> Option<usize> {
    let suffix = &text[from..];
    match (suffix.find("http://"), suffix.find("https://")) {
        (Some(http), Some(https)) => Some(from + http.min(https)),
        (Some(http), None) => Some(from + http),
        (None, Some(https)) => Some(from + https),
        (None, None) => None,
    }
}

fn is_bare_url_start_boundary(text: &str, start: usize) -> bool {
    start == 0
        || text[..start]
            .chars()
            .next_back()
            .map(|ch| !ch.is_alphanumeric() && ch != '_')
            .unwrap_or(true)
}

fn is_bare_url_terminator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '“' | '”' | '‘' | '’')
}

fn trim_bare_url_end(text: &str, start: usize, mut end: usize) -> usize {
    while let Some(ch) = text[start..end].chars().next_back() {
        let trim = matches!(
            ch,
            '.' | ','
                | '!'
                | '?'
                | ':'
                | ';'
                | '。'
                | '，'
                | '！'
                | '？'
                | '：'
                | '；'
                | '、'
                | '…'
        ) || matches!(ch, ')' | ']' | '}' | '）' | '】')
            && has_unbalanced_closer(&text[start..end], ch);
        if !trim {
            break;
        }
        end = end.saturating_sub(ch.len_utf8());
    }
    end
}

fn has_unbalanced_closer(text: &str, closer: char) -> bool {
    let opener = match closer {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        '）' => '（',
        '】' => '【',
        _ => return false,
    };
    text.chars().filter(|ch| *ch == closer).count()
        > text.chars().filter(|ch| *ch == opener).count()
}

/// Sum the displayed text length of every span in the buffer.
fn spans_text_len(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.len()).sum()
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.is_empty())
}

fn standalone_ordered_marker(spans: &[Span<'_>], style: Style) -> Option<Span<'static>> {
    let text = spans.iter().map(|s| s.content.as_ref()).collect::<String>();
    let text = text.trim_end();
    let number = text.strip_suffix(". ").or_else(|| text.strip_suffix('.'))?;
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(Span::styled(format!("{number}. "), style))
}

fn pending_marker_from_emitted_loose_item(
    lines: &mut Vec<Line<'static>>,
    style: Style,
) -> Option<Span<'static>> {
    if lines.len() < 2 || !line_is_blank(lines.last()?) {
        return None;
    }
    let marker = line_as_ordered_marker(&lines[lines.len() - 2], style)?;
    lines.pop();
    lines.pop();
    Some(marker)
}

fn line_is_standalone_list_marker(line: &Line<'_>) -> bool {
    line_as_ordered_marker(line, Style::new()).is_some()
}

fn line_as_ordered_marker(line: &Line<'_>, style: Style) -> Option<Span<'static>> {
    let text = line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
    let number = text
        .strip_suffix(". ")
        .or_else(|| text.trim_end().strip_suffix('.'))?;
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(Span::styled(format!("{number}. "), style))
}

fn pop_frame(stack: &mut Vec<InlineFrame>, pred: impl Fn(&InlineFrame) -> bool) {
    if let Some(pos) = stack.iter().rposition(pred) {
        stack.remove(pos);
    }
}

fn blockquote_kind_label(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "[NOTE]",
        BlockQuoteKind::Tip => "[TIP]",
        BlockQuoteKind::Important => "[IMPORTANT]",
        BlockQuoteKind::Warning => "[WARNING]",
        BlockQuoteKind::Caution => "[CAUTION]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn has_modifier(text: &Text<'static>, needle: &str, modifier: Modifier) -> bool {
        text.lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.contains(needle) && span.style.add_modifier.contains(modifier)
            })
        })
    }

    // ---------------------------------------------------------------
    // Happy-path structural coverage
    // ---------------------------------------------------------------

    #[test]
    fn empty_input_yields_empty_text() {
        let text = render_markdown("", &MarkdownTheme::plain());
        assert!(text.lines.is_empty());
    }

    #[test]
    fn whitespace_only_input_is_empty() {
        let text = render_markdown("   \n\n   ", &MarkdownTheme::plain());
        assert!(text.lines.is_empty());
    }

    #[test]
    fn plain_paragraph_renders_as_single_line() {
        let text = render_markdown("hello world", &MarkdownTheme::plain());
        assert_eq!(plain_lines(&text), vec!["hello world".to_string()]);
    }

    #[test]
    fn two_paragraphs_separated_by_blank_line() {
        let text = render_markdown("first\n\nsecond", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec!["first".to_string(), String::new(), "second".to_string()]
        );
    }

    #[test]
    fn soft_break_becomes_newline() {
        let text = render_markdown("line one\nline two", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec!["line one".to_string(), "line two".to_string()]
        );
    }

    #[test]
    fn hard_break_becomes_newline() {
        let text = render_markdown("a  \nb", &MarkdownTheme::plain());
        assert_eq!(plain_lines(&text), vec!["a".to_string(), "b".to_string()]);
    }

    // ---------------------------------------------------------------
    // Inline style coverage
    // ---------------------------------------------------------------

    #[test]
    fn strong_is_bold() {
        let theme = MarkdownTheme::plain();
        let text = render_markdown("**loud**", &theme);
        assert!(has_modifier(&text, "loud", Modifier::BOLD));
    }

    #[test]
    fn emphasis_is_italic() {
        let theme = MarkdownTheme::plain();
        let text = render_markdown("*soft*", &theme);
        assert!(has_modifier(&text, "soft", Modifier::ITALIC));
    }

    #[test]
    fn nested_strong_inside_emphasis_is_bold_italic() {
        let theme = MarkdownTheme::plain();
        let text = render_markdown("*outer **inner** outer*", &theme);
        let inner_bold_italic = text.lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.contains("inner")
                    && span.style.add_modifier.contains(Modifier::BOLD)
                    && span.style.add_modifier.contains(Modifier::ITALIC)
            })
        });
        assert!(inner_bold_italic);
    }

    #[test]
    fn inline_code_uses_code_style() {
        let theme = MarkdownTheme {
            code: Style::new().add_modifier(Modifier::REVERSED),
            ..MarkdownTheme::plain()
        };
        let text = render_markdown("use `foo()` here", &theme);
        let has_code = text.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.content == "foo()" && s.style.add_modifier.contains(Modifier::REVERSED))
        });
        assert!(has_code);
    }

    // ---------------------------------------------------------------
    // Block-level coverage
    // ---------------------------------------------------------------

    #[test]
    fn h1_uses_heading_style() {
        let theme = MarkdownTheme::plain();
        let text = render_markdown("# Title", &theme);
        assert!(has_modifier(&text, "Title", Modifier::UNDERLINED));
        assert!(has_modifier(&text, "Title", Modifier::BOLD));
    }

    #[test]
    fn h2_uses_bold_without_underline() {
        let theme = MarkdownTheme::plain();
        let text = render_markdown("## Subtitle", &theme);
        assert!(has_modifier(&text, "Subtitle", Modifier::BOLD));
        assert!(!has_modifier(&text, "Subtitle", Modifier::UNDERLINED));
    }

    #[test]
    fn unordered_list_renders_dash_markers() {
        let text = render_markdown("- one\n- two", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec!["- one".to_string(), "- two".to_string()]
        );
    }

    #[test]
    fn ordered_list_renders_numeric_markers_incrementing() {
        let text = render_markdown("1. one\n2. two\n3. three", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec![
                "1. one".to_string(),
                "2. two".to_string(),
                "3. three".to_string()
            ]
        );
    }

    #[test]
    fn ordered_list_respects_non_one_start() {
        let text = render_markdown("5. five\n6. six", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec!["5. five".to_string(), "6. six".to_string()]
        );
    }

    #[test]
    fn loose_ordered_list_keeps_marker_with_first_paragraph_text() {
        let text = render_markdown(
            "1.\n\n  c1cf4de Keep collapsed read/search hints visible",
            &MarkdownTheme::plain(),
        );
        let lines = plain_lines(&text);
        assert_eq!(
            lines,
            vec!["1. c1cf4de Keep collapsed read/search hints visible".to_string()]
        );
        assert!(!lines.iter().any(|line| line == "1." || line == "1. "));
    }

    #[test]
    fn nested_ordered_list_keeps_markers_with_text() {
        let text = render_markdown("1. one\n   1. nested\n2. two", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert!(lines.iter().any(|l| l == "1. one"));
        assert!(lines.iter().any(|l| l == "  1. nested"));
        assert!(lines.iter().any(|l| l == "2. two"));
    }

    #[test]
    fn nested_list_indents_with_two_spaces_per_level() {
        let text = render_markdown("- a\n  - b\n    - c", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert!(lines.iter().any(|l| l == "- a"));
        assert!(lines.iter().any(|l| l == "  - b"));
        assert!(lines.iter().any(|l| l == "    - c"));
    }

    #[test]
    fn blockquote_prefixes_bar() {
        let text = render_markdown("> wisdom", &MarkdownTheme::plain());
        let first = plain_lines(&text).into_iter().next().unwrap();
        assert!(first.starts_with(BLOCKQUOTE_BAR), "{first:?}");
    }

    #[test]
    fn blockquote_after_paragraph_has_single_blank_separator() {
        let text = render_markdown("intro\n\n> wisdom", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec!["intro".to_string(), String::new(), "│ wisdom".to_string()]
        );
    }

    #[test]
    fn blockquote_does_not_suppress_following_paragraph_separator() {
        let text = render_markdown("> wisdom\n\noutro", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&text),
            vec!["│ wisdom".to_string(), String::new(), "outro".to_string()]
        );
    }

    #[test]
    fn code_block_preserves_lines() {
        let text = render_markdown("```\nlet x = 1;\nlet y = 2;\n```", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert!(lines.iter().any(|l| l == "let x = 1;"));
        assert!(lines.iter().any(|l| l == "let y = 2;"));
    }

    #[test]
    fn code_block_uses_background_and_light_keyword_highlight() {
        let text = render_markdown(
            "```rust\nfn main() { return; }\n```",
            &MarkdownTheme::plain(),
        );
        let code_line = text
            .lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains("main")));
        let Some(code_line) = code_line else {
            panic!("missing highlighted code line: {:?}", plain_lines(&text));
        };
        assert!(code_line.spans.iter().all(|span| span.style.bg.is_some()));
        assert!(code_line.spans.iter().any(|span| {
            span.content == "fn" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn code_block_does_not_add_blank_separator_lines() {
        let text = render_markdown(
            "before\n```rust\nfn x() {}\n```\nafter",
            &MarkdownTheme::plain(),
        );
        assert_eq!(
            plain_lines(&text),
            vec![
                "before".to_string(),
                "fn x() {}".to_string(),
                "after".to_string()
            ]
        );
    }

    #[test]
    fn indented_code_block_preserves_lines() {
        let text = render_markdown("    let x = 1;\n    let y = 2;", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert_eq!(
            lines,
            vec!["let x = 1;".to_string(), "let y = 2;".to_string()]
        );
    }

    #[test]
    fn fenced_text_code_block_hides_language_marker_and_fences() {
        let text = render_markdown("```text\nhello\n```", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert_eq!(lines, vec!["hello".to_string()]);
        let joined = lines.join("\n");
        assert!(!joined.contains("‹text›"));
        assert!(!joined.contains("<text>"));
        assert!(!joined.contains("```"));
    }

    #[test]
    fn fenced_rust_code_block_hides_language_marker_but_keeps_code() {
        let text = render_markdown("```rust\nfn x() {}\n```", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert_eq!(lines, vec!["fn x() {}".to_string()]);
        assert!(!lines
            .iter()
            .any(|l| l == "rust" || l == "‹rust›" || l == "<rust>"));
    }

    #[test]
    fn hr_rule_emits_dashes() {
        let text = render_markdown("before\n\n---\n\nafter", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert!(lines.iter().any(|l| l == "---"));
    }

    #[test]
    fn markdown_link_renders_only_label_with_range_and_link_style() {
        let rendered =
            render_markdown_annotated("[click](https://example.com)", &MarkdownTheme::plain());
        assert_eq!(plain_lines(&rendered.text), vec!["click".to_string()]);
        assert_eq!(
            rendered.hyperlinks,
            vec![HyperlinkRange {
                line: 0,
                start_byte: 0,
                end_byte: "click".len(),
                target: "https://example.com".to_string(),
            }]
        );
        assert_eq!(rendered.text.lines[0].spans[0].style.fg, Some(Color::Blue));
    }

    #[test]
    fn empty_markdown_link_uses_target_as_visible_label() {
        let rendered =
            render_markdown_annotated("[](https://example.com)", &MarkdownTheme::plain());
        assert_eq!(
            plain_lines(&rendered.text),
            vec!["https://example.com".to_string()]
        );
        assert_eq!(rendered.hyperlinks.len(), 1);
        assert_eq!(rendered.hyperlinks[0].target, "https://example.com");
    }

    #[test]
    fn markdown_link_ranges_cross_soft_break_and_multiple_spans() {
        let rendered = render_markdown_annotated(
            "[one **two**\nthree](https://example.com)",
            &MarkdownTheme::plain(),
        );
        assert_eq!(
            plain_lines(&rendered.text),
            vec!["one two".to_string(), "three".to_string()]
        );
        assert_eq!(
            rendered.hyperlinks,
            vec![
                HyperlinkRange {
                    line: 0,
                    start_byte: 0,
                    end_byte: "one two".len(),
                    target: "https://example.com".to_string(),
                },
                HyperlinkRange {
                    line: 1,
                    start_byte: 0,
                    end_byte: "three".len(),
                    target: "https://example.com".to_string(),
                },
            ]
        );
    }

    #[test]
    fn blockquote_link_range_includes_visible_prefix_offset() {
        let rendered =
            render_markdown_annotated("> [文档](https://example.com)", &MarkdownTheme::plain());
        assert_eq!(plain_lines(&rendered.text), vec!["│ 文档".to_string()]);
        assert_eq!(rendered.hyperlinks[0].line, 0);
        assert_eq!(rendered.hyperlinks[0].start_byte, BLOCKQUOTE_BAR.len());
        assert_eq!(
            rendered.hyperlinks[0].end_byte,
            BLOCKQUOTE_BAR.len() + "文档".len()
        );
    }

    #[test]
    fn bare_urls_trim_sentence_punctuation_and_keep_balanced_parentheses() {
        let rendered = render_markdown_annotated(
            "See https://example.com/a(b)). Then `https://code.test`.\n\n```\nhttps://fenced.test\n```",
            &MarkdownTheme::plain(),
        );
        assert_eq!(rendered.hyperlinks.len(), 1);
        let link = &rendered.hyperlinks[0];
        assert_eq!(link.target, "https://example.com/a(b)");
        let line = plain_lines(&rendered.text)[link.line].clone();
        assert_eq!(&line[link.start_byte..link.end_byte], link.target);
        assert!(line.contains("https://example.com/a(b))."));
    }

    #[test]
    fn link_with_url_as_text_renders_once() {
        let rendered = render_markdown_annotated("<https://example.com>", &MarkdownTheme::plain());
        let joined = plain_lines(&rendered.text).join("\n");
        let count = joined.matches("https://example.com").count();
        assert_eq!(count, 1);
        assert_eq!(rendered.hyperlinks.len(), 1);
        assert_eq!(rendered.hyperlinks[0].target, "https://example.com");
    }

    #[test]
    fn reserved_prompt_tags_are_stripped() {
        let text = render_markdown(
            "<context>should be hidden</context>visible",
            &MarkdownTheme::plain(),
        );
        let joined = plain_lines(&text).join("\n");
        assert!(!joined.contains("should be hidden"));
        assert!(joined.contains("visible"));
    }

    #[test]
    fn table_link_keeps_label_without_appending_target() {
        let rendered = render_markdown_annotated(
            "| Link | Value |\n|---|---|\n| [docs](https://example.com) | 1 |",
            &MarkdownTheme::plain(),
        );
        let joined = plain_lines(&rendered.text).join("\n");
        assert!(joined.contains("docs"));
        assert!(!joined.contains("https://example.com"));
        assert_eq!(rendered.hyperlinks.len(), 1);
        let link = &rendered.hyperlinks[0];
        assert_eq!(link.target, "https://example.com");
        let line = rendered.text.lines[link.line]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(&line[link.start_byte..link.end_byte], "docs");
        assert!(rendered.text.lines[link.line].spans.iter().any(|span| {
            span.content.contains("docs") && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
    }

    #[test]
    fn table_bare_url_and_inline_code_keep_annotations_and_styles() {
        let rendered = render_markdown_annotated(
            "| URL | Code |\n|---|---|\n| https://example.com/path | `value` |",
            &MarkdownTheme::plain(),
        );
        assert_eq!(rendered.hyperlinks.len(), 1);
        assert_eq!(rendered.hyperlinks[0].target, "https://example.com/path");
        let joined = plain_lines(&rendered.text).join("\n");
        assert!(joined.contains("https://example.com/path"));
        assert!(joined.contains("value"));
        assert!(!joined.contains("`value`"));
        assert!(rendered.text.lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.contains("value") && span.style == MarkdownTheme::plain().code
            })
        }));
    }

    #[test]
    fn wrapped_table_links_emit_only_visible_content_ranges() {
        let mut marker_registry = Vec::new();
        let marker_cell = TableCell {
            plain: "abcdefghij中文".to_string(),
            annotations: vec![TableCellAnnotation {
                start_byte: 0,
                end_byte: "abcdefghij中文".len(),
                style: MarkdownTheme::plain().link,
                target: Some("https://example.com/long".to_string()),
            }],
            ..TableCell::default()
        };
        let marker_lines = wrap_marked_table_cell(&marker_cell, &mut marker_registry, 8, true);
        assert!(marker_lines.len() > 1, "{marker_lines:?}");
        assert!(
            marker_lines.iter().all(|line| {
                line.contains(TABLE_MARKER_START) && line.contains(TABLE_MARKER_END)
            }),
            "{marker_lines:?}"
        );
        let mut active = Vec::new();
        let mut bold = false;
        let (marker_line, marker_links) = render_marked_table_line(
            &format!("│ {} │", marker_lines[0]),
            MarkdownTheme::plain().text,
            &marker_registry,
            &mut active,
            &mut bold,
            0,
        );
        let marker_visible = marker_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            &marker_visible[marker_links[0].start_byte..marker_links[0].end_byte],
            "abcdefgh"
        );

        let rendered = render_markdown_annotated_with_width(
            "| Link |\n|---|\n| [abcdefghij中文](https://example.com/long) |",
            &MarkdownTheme::plain(),
            16,
        );
        assert!(!rendered.hyperlinks.is_empty());
        assert!(rendered
            .hyperlinks
            .iter()
            .all(|link| link.target == "https://example.com/long"));
        for link in &rendered.hyperlinks {
            let line = rendered.text.lines[link.line]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let linked = &line[link.start_byte..link.end_byte];
            assert!(!linked.trim().is_empty());
            assert!(!linked.contains('│'), "{linked:?} in {line:?}");
        }
        let joined = plain_lines(&rendered.text).join("\n");
        assert!(!joined.contains(TABLE_MARKER_BOUNDARY));
        assert!(!joined.contains(TABLE_MARKER_START));
        assert!(!joined.contains(TABLE_MARKER_END));
        assert!(!joined.contains(TABLE_MARKER_ZERO));
        assert!(!joined.contains(TABLE_MARKER_ONE));
    }

    #[test]
    fn table_renders_dynamic_boxed_grid() {
        let text = render_markdown("| A | B |\n|---|---|\n| 1 | 2 |", &MarkdownTheme::plain());
        let lines = plain_lines(&text);
        assert!(lines.iter().any(|l| l.starts_with("┌") && l.ends_with("┐")));
        assert!(lines
            .iter()
            .any(|l| l.contains("│") && l.contains("A") && l.contains("B")));
        assert!(lines
            .iter()
            .any(|l| l.contains("│") && l.contains("1") && l.contains("2")));
    }

    // ---------------------------------------------------------------
    // classification table — one source of truth for
    // "does renderer produce the expected plain-text shape".
    // ---------------------------------------------------------------

    #[test]
    fn plain_text_shape_table() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            ("plain", &["plain"]),
            ("**b**", &["b"]),
            ("*i*", &["i"]),
            ("`c`", &["c"]),
            ("# H1", &["H1"]),
            ("## H2", &["H2"]),
            ("- a\n- b", &["- a", "- b"]),
            ("1. a\n2. b", &["1. a", "2. b"]),
            ("> quoted", &["│ quoted"]),
            ("---", &["---"]),
            ("para1\n\npara2", &["para1", "", "para2"]),
        ];
        for (input, expected) in cases {
            let text = render_markdown(input, &MarkdownTheme::plain());
            let got = plain_lines(&text);
            let expected_vec: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, expected_vec, "table mismatch for {input:?}");
        }
    }

    // ---------------------------------------------------------------
    // Opt-in math coverage matrix
    //
    // mode       delimiters         pipeline result                  sidecars
    // off        all               historical Markdown path          none
    // on/valid   $/$$/\(...\)/\[...\] scanner -> core -> blocks     formula
    // on/failure any               exact code-styled source          none
    // ---------------------------------------------------------------

    fn code_styled_text(rendered: &RenderedMarkdown) -> String {
        rendered
            .text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style == MarkdownTheme::plain().code)
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn math_is_opt_in_and_default_off_for_both_delimiters() {
        let source = "inline $x^2$ and display $$y = 2$$";
        let rendered = render_markdown_annotated(source, &MarkdownTheme::plain());
        assert_eq!(plain_lines(&rendered.text), vec![source.to_string()]);
        assert!(rendered.formulas.is_empty());
    }

    #[test]
    fn valid_inline_and_display_math_produce_offline_assets_and_half_blocks() {
        let rendered = render_markdown_annotated_with_width_and_options(
            "inline $x^2$\n\n$$\\frac{1}{2}$$",
            &MarkdownTheme::plain(),
            48,
            MarkdownRenderOptions::math(),
        );
        assert_eq!(rendered.formulas.len(), 2);
        assert_eq!(rendered.formulas[0].display, FormulaDisplayMode::Inline);
        assert_eq!(rendered.formulas[0].source, "$x^2$");
        assert_eq!(rendered.formulas[0].terminal_rows, 1);
        assert_eq!(rendered.formulas[1].display, FormulaDisplayMode::Display);
        for formula in &rendered.formulas {
            assert!(formula.asset.svg.starts_with("<svg"));
            assert!(!formula.asset.bitmap.rgba.is_empty());
            assert!(formula
                .asset
                .bitmap
                .rgba
                .chunks_exact(4)
                .any(|pixel| pixel[3] != 0));
            assert!(
                u64::from(formula.asset.bitmap.width) * u64::from(formula.asset.bitmap.height)
                    <= MAX_FORMULA_BITMAP_PIXELS
            );
            assert!((1..=MAX_FORMULA_TERMINAL_ROWS).contains(&formula.terminal_rows));
            assert_eq!(formula.ranges.len(), formula.terminal_rows as usize);
            let fallback = plain_lines(&formula.fallback).join("\n");
            assert!(fallback.chars().any(|ch| matches!(ch, '▀' | '▄')));
        }
    }

    #[test]
    fn backslash_delimiters_use_shared_scanner_ranges_and_preserve_modes() {
        let source = r"inline \(x + 1\)

\[\frac{1}{2}\]";
        let rendered = render_markdown_annotated_with_width_and_options(
            source,
            &MarkdownTheme::plain(),
            48,
            MarkdownRenderOptions::math(),
        );
        assert_eq!(rendered.formulas.len(), 2);
        assert_eq!(rendered.formulas[0].source, r"\(x + 1\)");
        assert_eq!(rendered.formulas[0].expression, "x + 1");
        assert_eq!(rendered.formulas[0].display, FormulaDisplayMode::Inline);
        assert_eq!(rendered.formulas[1].source, r"\[\frac{1}{2}\]");
        assert_eq!(rendered.formulas[1].expression, r"\frac{1}{2}");
        assert_eq!(rendered.formulas[1].display, FormulaDisplayMode::Display);
    }

    #[test]
    fn escaped_dollars_currency_and_code_do_not_steal_formula_delimiters() {
        let source = "cost \\$5.00, then $x+1$, code `$y$`\n\n```text\n$z$\n```";
        let rendered = render_markdown_annotated_with_options(
            source,
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );

        assert_eq!(rendered.formulas.len(), 1);
        assert_eq!(rendered.formulas[0].expression, "x+1");
        let visible = plain_lines(&rendered.text).join("\n");
        assert!(visible.contains("$5.00"), "{visible:?}");
        assert!(visible.contains("$y$"), "{visible:?}");
        assert!(visible.contains("$z$"), "{visible:?}");

        let currency_source = "email a~b, cost $5.00 and formula $x$";
        let currency = render_markdown_annotated_with_options(
            currency_source,
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert_eq!(currency.formulas.len(), 1);
        assert_eq!(currency.formulas[0].expression, "x");
        let currency_visible = plain_lines(&currency.text).join("\n");
        assert!(currency_visible.contains("$5.00"));
        assert!(currency_visible.contains("a~b"));
    }

    #[test]
    fn math_enabled_preserves_currency_dollars_in_link_and_image_destinations() {
        let link_target = "https://example.test/a~b?amount=$5.00";
        let link = render_markdown_annotated_with_options(
            &format!("[price]({link_target})"),
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert_eq!(link.hyperlinks.len(), 1);
        assert_eq!(link.hyperlinks[0].target, link_target);

        let image_target = "https://example.test/r~s?amount=$7.00";
        let image = render_markdown_annotated_with_options(
            &format!("![receipt]({image_target})"),
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert!(plain_lines(&image.text).join("\n").contains(image_target));
    }

    #[test]
    fn math_enabled_does_not_normalize_backslash_delimiters_in_destinations() {
        let link_source = r"[literal](https://example.test/\(segment\))";
        let default_link = render_markdown_annotated(link_source, &MarkdownTheme::plain());
        let math_link = render_markdown_annotated_with_options(
            link_source,
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert_eq!(math_link.hyperlinks, default_link.hyperlinks);
        assert!(math_link.formulas.is_empty());

        let image_source = r"![literal](https://example.test/\[segment\])";
        let default_image = render_markdown_annotated(image_source, &MarkdownTheme::plain());
        let math_image = render_markdown_annotated_with_options(
            image_source,
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert_eq!(math_image.text, default_image.text);
        assert!(math_image.formulas.is_empty());
    }

    #[test]
    fn common_formula_families_render_without_external_tools() {
        for expression in [
            r"\sqrt{x}",
            r"\sum_{i=1}^{n} i",
            r"\int_0^1 x^2 \, dx",
            r"\begin{matrix}a & b \\ c & d\end{matrix}",
        ] {
            let source = format!("$${expression}$$");
            let rendered = render_markdown_annotated_with_options(
                &source,
                &MarkdownTheme::plain(),
                MarkdownRenderOptions::math(),
            );
            assert_eq!(rendered.formulas.len(), 1, "{expression}");
            assert!(!rendered.formulas[0].asset.bitmap.rgba.is_empty());
        }
    }

    #[test]
    fn formula_assets_and_unicode_fallback_are_deterministic() {
        let render = || {
            render_markdown_annotated_with_options(
                "$\\sqrt{x_1 + y^2}$",
                &MarkdownTheme::plain(),
                MarkdownRenderOptions::math(),
            )
        };
        let first = render();
        let second = render();
        assert_eq!(first.text, second.text);
        assert_eq!(first.formulas, second.formulas);
    }

    #[test]
    fn hyperlinks_and_formula_sidecars_coexist() {
        let rendered = render_markdown_annotated_with_options(
            "[docs](https://example.test) and $x + 1$",
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert_eq!(rendered.hyperlinks.len(), 1);
        assert_eq!(rendered.hyperlinks[0].target, "https://example.test");
        assert_eq!(rendered.formulas.len(), 1);
        assert!(!rendered.formulas[0].ranges.is_empty());
    }

    #[test]
    fn invalid_unclosed_unsupported_and_oversized_math_preserve_raw_code_source() {
        let oversized = format!("${}$", "x".repeat(MAX_FORMULA_SOURCE_BYTES));
        let cases = [
            "$\\definitely_not_a_ratex_command{x}$".to_string(),
            "unclosed $x + 1".to_string(),
            "unclosed $x and **raw** [label](https://example.test)".to_string(),
            oversized,
        ];
        for source in cases {
            let rendered = render_markdown_annotated_with_options(
                &source,
                &MarkdownTheme::plain(),
                MarkdownRenderOptions::math(),
            );
            assert!(rendered.formulas.is_empty(), "{source:?}");
            assert_eq!(plain_lines(&rendered.text).join("\n"), source, "{source:?}");
            assert!(code_styled_text(&rendered).contains('$'), "{source:?}");
        }

        let table = render_markdown_annotated_with_options(
            "| formula |\n|---|\n| $x$ |",
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert!(table.formulas.is_empty());
        assert!(plain_lines(&table.text).join("\n").contains("$x$"));
        assert!(code_styled_text(&table).contains("$x$"));

        let unclosed_table = render_markdown_annotated_with_options(
            "| formula |\n|---|\n| $x |",
            &MarkdownTheme::plain(),
            MarkdownRenderOptions::math(),
        );
        assert!(unclosed_table.formulas.is_empty());
        assert!(plain_lines(&unclosed_table.text).join("\n").contains("$x"));
        assert!(code_styled_text(&unclosed_table).contains("$x"));
    }

    #[test]
    fn formula_resource_limits_reject_before_raster_allocation() {
        let oversized_source = format!("${}$", "x".repeat(MAX_FORMULA_SOURCE_BYTES));
        assert!(matches!(
            prepare_formula(
                &oversized_source[1..oversized_source.len() - 1],
                &oversized_source,
                FormulaDisplayMode::Inline,
                80,
                None,
            ),
            Err(FormulaRenderError::SourceTooLarge)
        ));
        assert!(matches!(
            prepare_formula(
                "\\rule{100000em}{100000em}",
                "$$\\rule{100000em}{100000em}$$",
                FormulaDisplayMode::Display,
                u16::MAX as usize,
                None,
            ),
            Err(FormulaRenderError::BitmapTooLarge)
        ));
        assert!(matches!(
            prepare_formula(
                "\\rule{1em}{1000em}",
                "$$\\rule{1em}{1000em}$$",
                FormulaDisplayMode::Display,
                80,
                None,
            ),
            Err(FormulaRenderError::TooManyRows)
        ));
    }

    // ---------------------------------------------------------------
    // MarkdownTheme construction
    // ---------------------------------------------------------------

    #[test]
    fn markdown_theme_from_messages_inherits_text_style() {
        let messages = MessagesRenderTheme::plain();
        let theme = MarkdownTheme::from_messages(&messages);
        assert_eq!(theme.text, messages.text);
        assert!(theme.strong.add_modifier.contains(Modifier::BOLD));
        assert!(theme.emphasis.add_modifier.contains(Modifier::ITALIC));
        assert!(theme.heading.add_modifier.contains(Modifier::BOLD));
        assert!(theme.heading.add_modifier.contains(Modifier::UNDERLINED));
    }
}
