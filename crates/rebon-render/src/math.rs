//! Shared Markdown math-fragment scanning.

use std::ops::Range;

/// Delimiter pair surrounding a math fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MathDelimiter {
    /// `$...$`.
    Dollar,
    /// `$$...$$`.
    DoubleDollar,
    /// `\(...\)`.
    Parentheses,
    /// `\[...\]`.
    Brackets,
}

/// Inline versus display placement inferred from the delimiters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MathDisplayMode {
    /// Inline math from `$...$` or `\(...\)`.
    Inline,
    /// Display math from `$$...$$` or `\[...\]`.
    Display,
}

/// One closed math fragment addressing byte ranges in the original Markdown.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MathFragment {
    /// Range including the opening and closing delimiters.
    pub source_range: Range<usize>,
    /// Range containing only the formula expression.
    pub expression_range: Range<usize>,
    /// Exact delimiter syntax used by the source.
    pub delimiter: MathDelimiter,
    /// Inline or display placement inferred from [`Self::delimiter`].
    pub display: MathDisplayMode,
}

impl MathFragment {
    /// Read the complete delimited source from the scanned Markdown.
    pub fn source<'a>(&self, markdown: &'a str) -> &'a str {
        &markdown[self.source_range.clone()]
    }

    /// Read the delimiter-free expression from the scanned Markdown.
    pub fn expression<'a>(&self, markdown: &'a str) -> &'a str {
        &markdown[self.expression_range.clone()]
    }
}

#[derive(Debug, Clone, Copy)]
struct DelimiterSpec {
    delimiter: MathDelimiter,
    display: MathDisplayMode,
    open: &'static [u8],
    close: &'static [u8],
}

const DOUBLE_DOLLAR: DelimiterSpec = DelimiterSpec {
    delimiter: MathDelimiter::DoubleDollar,
    display: MathDisplayMode::Display,
    open: b"$$",
    close: b"$$",
};
const DOLLAR: DelimiterSpec = DelimiterSpec {
    delimiter: MathDelimiter::Dollar,
    display: MathDisplayMode::Inline,
    open: b"$",
    close: b"$",
};
const PARENTHESES: DelimiterSpec = DelimiterSpec {
    delimiter: MathDelimiter::Parentheses,
    display: MathDisplayMode::Inline,
    open: b"\\(",
    close: b"\\)",
};
const BRACKETS: DelimiterSpec = DelimiterSpec {
    delimiter: MathDelimiter::Brackets,
    display: MathDisplayMode::Display,
    open: b"\\[",
    close: b"\\]",
};

/// Scan closed math fragments without interpreting formula contents.
///
/// Fenced code blocks, inline code spans, escaped delimiters, and dollar signs
/// that look like currency are ignored. Returned ranges always address the
/// original UTF-8 input, are non-overlapping, and appear in source order.
pub fn scan_math_fragments(markdown: &str) -> Vec<MathFragment> {
    let protected = code_ranges(markdown);
    let bytes = markdown.as_bytes();
    let mut fragments = Vec::new();
    let mut index = 0usize;
    let mut protected_index = 0usize;

    while index < bytes.len() {
        while protected_index < protected.len() && protected[protected_index].end <= index {
            protected_index += 1;
        }
        if let Some(range) = protected.get(protected_index) {
            if range.start <= index {
                index = range.end;
                continue;
            }
        }

        let Some(spec) = opening_delimiter(markdown, index) else {
            index += 1;
            continue;
        };
        if spec.delimiter == MathDelimiter::Dollar && is_currency_dollar(markdown, index) {
            index += 1;
            continue;
        }

        let expression_start = index + spec.open.len();
        let Some(close_start) = find_closing_delimiter(
            markdown,
            expression_start,
            spec,
            &protected,
            protected_index,
        ) else {
            index += spec.open.len();
            continue;
        };
        if markdown[expression_start..close_start].trim().is_empty() {
            index = close_start + spec.close.len();
            continue;
        }

        let source_end = close_start + spec.close.len();
        fragments.push(MathFragment {
            source_range: index..source_end,
            expression_range: expression_start..close_start,
            delimiter: spec.delimiter,
            display: spec.display,
        });
        index = source_end;
    }

    fragments
}

fn opening_delimiter(markdown: &str, index: usize) -> Option<DelimiterSpec> {
    let bytes = markdown.as_bytes();
    if is_backslash_escaped(bytes, index) {
        return None;
    }
    let tail = &bytes[index..];
    if tail.starts_with(DOUBLE_DOLLAR.open) {
        return Some(DOUBLE_DOLLAR);
    }
    if tail.starts_with(DOLLAR.open)
        && bytes.get(index.wrapping_sub(1)) != Some(&b'$')
        && bytes.get(index + 1) != Some(&b'$')
        && !bytes.get(index + 1).is_some_and(u8::is_ascii_whitespace)
        && !bytes
            .get(index.wrapping_sub(1))
            .is_some_and(u8::is_ascii_digit)
    {
        return Some(DOLLAR);
    }
    if tail.starts_with(PARENTHESES.open) {
        return Some(PARENTHESES);
    }
    if tail.starts_with(BRACKETS.open) {
        return Some(BRACKETS);
    }
    None
}

fn find_closing_delimiter(
    markdown: &str,
    mut index: usize,
    spec: DelimiterSpec,
    protected: &[Range<usize>],
    mut protected_index: usize,
) -> Option<usize> {
    let bytes = markdown.as_bytes();
    while index + spec.close.len() <= bytes.len() {
        while protected_index < protected.len() && protected[protected_index].end <= index {
            protected_index += 1;
        }
        if let Some(range) = protected.get(protected_index) {
            if range.start <= index {
                return None;
            }
        }
        if spec.display == MathDisplayMode::Inline && bytes[index] == b'\n' {
            return None;
        }
        if bytes[index..].starts_with(spec.close) && !is_backslash_escaped(bytes, index) {
            if spec.delimiter == MathDelimiter::Dollar
                && (bytes.get(index + 1) == Some(&b'$')
                    || bytes.get(index.wrapping_sub(1)) == Some(&b'$')
                    || bytes
                        .get(index.wrapping_sub(1))
                        .is_some_and(u8::is_ascii_whitespace))
            {
                index += 1;
                continue;
            }
            return Some(index);
        }
        index += 1;
    }
    None
}

fn is_currency_dollar(markdown: &str, index: usize) -> bool {
    let bytes = markdown.as_bytes();
    if !bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
        return false;
    }

    let mut amount_end = index + 1;
    while amount_end < bytes.len() {
        match bytes[amount_end] {
            b'0'..=b'9' => amount_end += 1,
            b',' | b'.' if bytes.get(amount_end + 1).is_some_and(u8::is_ascii_digit) => {
                amount_end += 1;
            }
            _ => break,
        }
    }
    if bytes.get(amount_end) == Some(&b'$') && !is_backslash_escaped(bytes, amount_end) {
        return false;
    }

    let Some(relative_close) = markdown[amount_end..].find('$') else {
        return true;
    };
    let close = amount_end + relative_close;
    let candidate = &markdown[index + 1..close];
    !candidate.bytes().any(|byte| {
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
}

fn code_ranges(markdown: &str) -> Vec<Range<usize>> {
    let fenced = fenced_code_ranges(markdown);
    let mut ranges = fenced.clone();
    let mut cursor = 0usize;
    for fence in fenced {
        inline_code_ranges(&markdown[cursor..fence.start], cursor, &mut ranges);
        cursor = fence.end;
    }
    inline_code_ranges(&markdown[cursor..], cursor, &mut ranges);
    ranges.sort_by_key(|range| range.start);
    ranges
}

fn fenced_code_ranges(markdown: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut open: Option<(usize, u8, usize)> = None;
    let mut offset = 0usize;

    for line in markdown.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);
        if let Some((start, marker, minimum_len)) = open {
            if is_closing_fence(line_without_newline, marker, minimum_len) {
                ranges.push(start..offset + line.len());
                open = None;
            }
        } else if let Some((marker, length)) = opening_fence(line_without_newline) {
            open = Some((offset, marker, length));
        }
        offset += line.len();
    }
    if let Some((start, _, _)) = open {
        ranges.push(start..markdown.len());
    }
    ranges
}

fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let marker = *bytes.get(indent)?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = bytes[indent..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    if length < 3 || marker == b'`' && bytes[indent + length..].contains(&b'`') {
        return None;
    }
    Some((marker, length))
}

fn is_closing_fence(line: &str, marker: u8, minimum_len: usize) -> bool {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 || bytes.get(indent) != Some(&marker) {
        return false;
    }
    let length = bytes[indent..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    length >= minimum_len
        && bytes[indent + length..]
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

fn inline_code_ranges(segment: &str, base: usize, ranges: &mut Vec<Range<usize>>) {
    let bytes = segment.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'`' || is_backslash_escaped(bytes, index) {
            index += 1;
            continue;
        }
        let run = bytes[index..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        let mut search = index + run;
        let mut close = None;
        while search < bytes.len() {
            if bytes[search] != b'`' {
                search += 1;
                continue;
            }
            let candidate = bytes[search..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            if candidate == run {
                close = Some(search + candidate);
                break;
            }
            search += candidate;
        }
        if let Some(end) = close {
            ranges.push(base + index..base + end);
            index = end;
        } else {
            index += run;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fragments(markdown: &str) -> Vec<(&str, &str, MathDelimiter, MathDisplayMode)> {
        scan_math_fragments(markdown)
            .into_iter()
            .map(|fragment| {
                (
                    fragment.source(markdown),
                    fragment.expression(markdown),
                    fragment.delimiter,
                    fragment.display,
                )
            })
            .collect()
    }

    // Scanner coverage matrix:
    // four delimiter forms -> ordered byte ranges; fenced/inline code -> skipped;
    // escaped/currency/unclosed -> skipped; valid numeric math -> retained.

    #[test]
    fn recognizes_all_delimiters_in_source_order() {
        let markdown = "a $x+1$ b $$y^2$$ c \\(z\\) d \\[w\\]";
        assert_eq!(
            fragments(markdown),
            vec![
                (
                    "$x+1$",
                    "x+1",
                    MathDelimiter::Dollar,
                    MathDisplayMode::Inline
                ),
                (
                    "$$y^2$$",
                    "y^2",
                    MathDelimiter::DoubleDollar,
                    MathDisplayMode::Display,
                ),
                (
                    "\\(z\\)",
                    "z",
                    MathDelimiter::Parentheses,
                    MathDisplayMode::Inline,
                ),
                (
                    "\\[w\\]",
                    "w",
                    MathDelimiter::Brackets,
                    MathDisplayMode::Display,
                ),
            ]
        );
    }

    #[test]
    fn ranges_address_original_utf8_without_copying() {
        let markdown = "前置 \\(α + β\\) 后置";
        let scanned = scan_math_fragments(markdown);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].source(markdown), "\\(α + β\\)");
        assert_eq!(scanned[0].expression(markdown), "α + β");
        assert_eq!(&markdown[scanned[0].source_range.clone()], "\\(α + β\\)");
    }

    #[test]
    fn skips_backtick_and_tilde_fenced_code() {
        let markdown = concat!(
            "$before$\n\n",
            "```rust\n$x$ \\(y\\)\n```\n\n",
            "~~~\n$$z$$ \\[w\\]\n~~~~\n\n",
            "$after$"
        );
        assert_eq!(
            fragments(markdown)
                .into_iter()
                .map(|fragment| fragment.0)
                .collect::<Vec<_>>(),
            vec!["$before$", "$after$"]
        );
    }

    #[test]
    fn skips_indented_and_unclosed_fences_to_end_of_input() {
        let markdown = "   ```text\n$x$\n   ```\n$ok$\n~~~\n$never$";
        assert_eq!(
            fragments(markdown)
                .into_iter()
                .map(|fragment| fragment.0)
                .collect::<Vec<_>>(),
            vec!["$ok$"]
        );
    }

    #[test]
    fn backtick_fence_info_with_backtick_is_not_treated_as_fenced_code() {
        let markdown = "```lang`suffix\n$x$";
        assert_eq!(
            fragments(markdown)
                .into_iter()
                .map(|fragment| fragment.0)
                .collect::<Vec<_>>(),
            vec!["$x$"]
        );
    }

    #[test]
    fn skips_inline_code_with_matching_backtick_runs() {
        let markdown = "`$x$` ``code ` \\(y\\)`` and $z$";
        assert_eq!(
            fragments(markdown)
                .into_iter()
                .map(|fragment| fragment.0)
                .collect::<Vec<_>>(),
            vec!["$z$"]
        );
    }

    #[test]
    fn escaped_delimiters_and_escaped_closers_are_not_consumed() {
        let markdown = r"\$x$ \\(y\) \\[z\] $a \$ b$ \(c \\) d\)";
        assert_eq!(
            fragments(markdown),
            vec![
                (
                    "$a \\$ b$",
                    "a \\$ b",
                    MathDelimiter::Dollar,
                    MathDisplayMode::Inline,
                ),
                (
                    "\\(c \\\\) d\\)",
                    "c \\\\) d",
                    MathDelimiter::Parentheses,
                    MathDisplayMode::Inline,
                ),
            ]
        );
    }

    #[test]
    fn currency_is_skipped_without_hiding_later_math() {
        let markdown = "cost $5, $5.00, or $1,234.56; 10$ tip; math $x$ and $5+2$";
        assert_eq!(
            fragments(markdown)
                .into_iter()
                .map(|fragment| fragment.0)
                .collect::<Vec<_>>(),
            vec!["$x$", "$5+2$"]
        );
    }

    #[test]
    fn closed_numeric_formulas_are_not_misclassified_as_currency() {
        assert_eq!(
            fragments("$5$ $$42$$ \\(7\\)"),
            vec![
                ("$5$", "5", MathDelimiter::Dollar, MathDisplayMode::Inline),
                (
                    "$$42$$",
                    "42",
                    MathDelimiter::DoubleDollar,
                    MathDisplayMode::Display,
                ),
                (
                    "\\(7\\)",
                    "7",
                    MathDelimiter::Parentheses,
                    MathDisplayMode::Inline,
                ),
            ]
        );
    }

    #[test]
    fn unclosed_empty_and_multiline_inline_fragments_are_not_returned() {
        for markdown in ["$x", "$$x", "\\(x", "\\[x", "$$", "\\(  \\)", "$x\ny$"] {
            assert!(scan_math_fragments(markdown).is_empty(), "{markdown:?}");
        }
        assert_eq!(
            fragments("$$x\n+y$$ \\[a\n+b\\]")
                .into_iter()
                .map(|fragment| fragment.0)
                .collect::<Vec<_>>(),
            vec!["$$x\n+y$$", "\\[a\n+b\\]"]
        );
    }

    #[test]
    fn adjacent_fragments_remain_non_overlapping() {
        let markdown = "$x$\\(y\\)$$z$$";
        let scanned = scan_math_fragments(markdown);
        assert_eq!(scanned.len(), 3);
        assert!(scanned
            .windows(2)
            .all(|pair| pair[0].source_range.end <= pair[1].source_range.start));
    }
}
