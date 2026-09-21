//! Every shell tokeniser in the workspace lives here.
//!
//! Splitting a command string into words is the first step of every security
//! decision made about a shell, and a second implementation of it
//! is a second set of quoting rules. The gap between two such sets is not a
//! style problem: it is the place an evasion lives, because a command that
//! one of them reads as `rm -rf /` and the other reads as one opaque word gets
//! whichever answer the caller happened to ask for.
//!
//! Two shapes of scan are needed, and they are not interchangeable.
//!
//! * [`simple_command_segments`] answers *"is this provably a plain argv?"*
//!   for the permission-rule matchers and for `mkdir` target resolution.
//!   Anything it cannot account for stops the scan; the callers all treat a
//!   stopped scan as "matches no rule", so being unable to parse something is
//!   safe by construction. The callers differ only in a [`Grammar`],
//!   because their differences are real: a permission rule is written against
//!   the literal word a user typed, so `*` is an ordinary character there,
//!   while a `mkdir` target has to become a real path, so `*` has to stop it.
//!
//! * [`shell_tokens`] and [`workflow_shell_tokens`] answer *"what words does
//!   this command contain?"* for the static classifier, which must produce an
//!   answer for every input including ones no shell would accept. These two
//!   stay separate on purpose — see the note on [`workflow_shell_tokens`].

/// What a backslash inside double quotes does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DoubleQuoteEscape {
    /// The next character is taken literally whatever it is, so `"a\b"` is
    /// `ab`. POSIX disagrees, but the permission matchers have always read it
    /// this way and the rules users have already written match these words.
    Literal,
    /// POSIX: a backslash is only an escape before `$`, a backtick, `"` or
    /// itself, so `"C:\temp\x"` keeps its separators and names a real path.
    Posix,
}

/// Why a scan stopped without producing an argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bail {
    /// A character the scan refuses to interpret. This is the answer for
    /// everything a shell would do beyond running one command with arguments:
    /// pipelines, redirections, substitution, backgrounding, and — depending
    /// on the grammar — globbing and expansion.
    Metacharacter,
    /// The command ends inside a quote or on a dangling backslash.
    Unterminated,
    /// `&&` with no command on one side of it.
    EmptySegment,
}

/// The dialect knobs the callers disagree on. Every field exists because
/// changing it for one of them would change a permission verdict.
pub struct Grammar {
    /// Characters that stop the scan outside quotes, on top of the set every
    /// caller shares (`;`, `|`, `<`, `>`, a newline, a backtick, and `$(`).
    pub unquoted_metacharacters: &'static [char],
    /// Whether a bare `$` stops the scan, rather than only `$(`. A caller that
    /// has to turn a word into a path cannot expand a variable, so it must not
    /// pretend the unexpanded spelling is the target.
    pub dollar_is_metacharacter: bool,
    /// Whether a newline stops the scan even inside quotes or after a
    /// backslash. A caller that resolves paths wants one line; a caller that
    /// matches a rule against what the user typed keeps the newline as text.
    pub newline_always_ends_the_scan: bool,
    /// Whether `&&` starts a new segment instead of stopping the scan. A lone
    /// `&` always stops it: backgrounding is not something a rule can vouch
    /// for.
    pub and_chain_separates: bool,
    pub double_quote_escape: DoubleQuoteEscape,
    /// Whether `""` produces an empty word. It is a real argument, but the
    /// rule matchers have always dropped it.
    pub keep_empty_tokens: bool,
    /// Which characters end a word. ASCII-only is the conservative choice: a
    /// non-breaking space stays inside the word, so `echo\u{a0}rm` is one word
    /// that matches no `echo` rule rather than two that do.
    pub word_separator: fn(char) -> bool,
}

/// The permission-rule matchers' grammar: the literal words a user would have
/// written in a rule, with `&&` chains allowed because each link is checked.
pub const RULE_ARGV: Grammar = Grammar {
    unquoted_metacharacters: &['(', ')'],
    dollar_is_metacharacter: false,
    newline_always_ends_the_scan: false,
    and_chain_separates: true,
    double_quote_escape: DoubleQuoteEscape::Literal,
    keep_empty_tokens: false,
    word_separator: |ch| ch.is_ascii_whitespace(),
};

/// "Is there any shell metacharacter here at all?" — the same scan as
/// [`RULE_ARGV`] with `&&` no longer excused and grouping no longer refused,
/// because the callers of that question ask it about a command they are about
/// to offer as an editable prefix.
pub const ANY_METACHARACTER: Grammar = Grammar {
    unquoted_metacharacters: &[],
    and_chain_separates: false,
    ..RULE_ARGV
};

/// Resolving a word into a path the tool will actually create or delete.
/// Everything that could expand to something else stops the scan.
pub const STATIC_ARGV: Grammar = Grammar {
    unquoted_metacharacters: &['(', ')', '*', '?', '[', ']', '{', '}', '~', '#'],
    dollar_is_metacharacter: true,
    newline_always_ends_the_scan: true,
    and_chain_separates: false,
    double_quote_escape: DoubleQuoteEscape::Posix,
    keep_empty_tokens: true,
    word_separator: char::is_whitespace,
};

/// What a permission rule gets to match against.
///
/// The name is historical — [`crate::powershell_shape::parse_powershell_shape`]
/// answers with the same three cases, because a rule matcher only ever needs
/// to know "one command", "commands each of which I can check", or "I cannot
/// vouch for this".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashShape {
    Simple(Vec<String>),
    SafeAndChain(Vec<Vec<String>>),
    UnsafeComplex,
}

/// The argv a POSIX shell would run, or [`BashShape::UnsafeComplex`] if the
/// command does anything a rule cannot be checked against.
pub fn parse_bash_shape(command: &str) -> BashShape {
    let command = command.trim();
    if command.is_empty() {
        return BashShape::UnsafeComplex;
    }

    match simple_command_segments(command, &RULE_ARGV) {
        Ok(mut segments) => match segments.len() {
            0 => BashShape::UnsafeComplex,
            1 => BashShape::Simple(segments.remove(0)),
            _ => BashShape::SafeAndChain(segments),
        },
        Err(_) => BashShape::UnsafeComplex,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Split `command` into `&&`-separated segments of words, or say why it could
/// not be done. A `command` this returns `Ok` for runs exactly the programs
/// named in the segments with exactly the arguments listed — that is the
/// property the callers rely on, and it is why every doubtful character is an
/// `Err` instead of a guess.
pub fn simple_command_segments(command: &str, grammar: &Grammar) -> Result<Vec<Vec<String>>, Bail> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut token_started = false;
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if grammar.newline_always_ends_the_scan && matches!(ch, '\n' | '\r') {
            return Err(Bail::Metacharacter);
        }
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    token.push(ch);
                }
            }
            Quote::Double => {
                if escaped {
                    token.push(ch);
                    escaped = false;
                    continue;
                }
                match ch {
                    '\\' => match grammar.double_quote_escape {
                        DoubleQuoteEscape::Literal => escaped = true,
                        DoubleQuoteEscape::Posix => {
                            let Some(next) = chars.next() else {
                                return Err(Bail::Unterminated);
                            };
                            if grammar.newline_always_ends_the_scan && matches!(next, '\n' | '\r') {
                                return Err(Bail::Metacharacter);
                            }
                            if matches!(next, '$' | '`' | '"' | '\\') {
                                token.push(next);
                            } else {
                                token.push('\\');
                                token.push(next);
                            }
                        }
                    },
                    '"' => quote = Quote::None,
                    '`' => return Err(Bail::Metacharacter),
                    '$' if grammar.dollar_is_metacharacter => return Err(Bail::Metacharacter),
                    '$' if chars.peek() == Some(&'(') => return Err(Bail::Metacharacter),
                    _ => token.push(ch),
                }
            }
            Quote::None => {
                if escaped {
                    token.push(ch);
                    token_started = true;
                    escaped = false;
                    continue;
                }
                if grammar.unquoted_metacharacters.contains(&ch) {
                    return Err(Bail::Metacharacter);
                }
                match ch {
                    '\\' => escaped = true,
                    '\'' => {
                        quote = Quote::Single;
                        token_started = true;
                    }
                    '"' => {
                        quote = Quote::Double;
                        token_started = true;
                    }
                    '\n' | '\r' | ';' | '|' | '<' | '>' | '`' => {
                        return Err(Bail::Metacharacter);
                    }
                    '$' if grammar.dollar_is_metacharacter => return Err(Bail::Metacharacter),
                    '$' if chars.peek() == Some(&'(') => return Err(Bail::Metacharacter),
                    '&' => {
                        if !grammar.and_chain_separates || chars.peek() != Some(&'&') {
                            return Err(Bail::Metacharacter);
                        }
                        chars.next();
                        push_word(&mut current, &mut token, &mut token_started, grammar);
                        if current.is_empty() {
                            return Err(Bail::EmptySegment);
                        }
                        segments.push(std::mem::take(&mut current));
                    }
                    ch if (grammar.word_separator)(ch) => {
                        push_word(&mut current, &mut token, &mut token_started, grammar);
                    }
                    _ => {
                        token.push(ch);
                        token_started = true;
                    }
                }
            }
        }
    }

    if escaped || quote != Quote::None {
        return Err(Bail::Unterminated);
    }
    push_word(&mut current, &mut token, &mut token_started, grammar);
    if grammar.and_chain_separates && current.is_empty() {
        return Err(Bail::EmptySegment);
    }
    segments.push(current);
    Ok(segments)
}

fn push_word(current: &mut Vec<String>, token: &mut String, started: &mut bool, grammar: &Grammar) {
    if !token.is_empty() || (grammar.keep_empty_tokens && *started) {
        current.push(std::mem::take(token));
    }
    token.clear();
    *started = false;
}

/// The classifier's plain word split: quotes and backslash escapes, nothing
/// else. It deliberately leaves `(`, `)`, `{`, `}` and a backtick inside the
/// word, because the PowerShell half of the classifier reads whole method
/// calls — `[System.IO.FileInfo]::new("old.txt").Delete()` has to survive as
/// one token for the deletion detector to see the `.Delete()` on the end of
/// it. Feeding those commands through [`workflow_shell_tokens`] instead loses
/// that detection, which is why the two are not merged.
pub(crate) fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else if ch == '\\' && q == '"' {
                if let Some(next) = chars.next() {
                    if matches!(next, '"' | '\\') {
                        current.push(next);
                    } else {
                        current.push('\\');
                        current.push(next);
                    }
                }
            } else {
                current.push(ch);
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => {
                if let Some(next) = chars.next() {
                    if next.is_whitespace() || matches!(next, '\'' | '"' | '\\' | '&' | '|' | ';') {
                        current.push(next);
                    } else {
                        current.push('\\');
                        current.push(next);
                    }
                } else {
                    current.push('\\');
                }
            }
            '&' | '|' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                if chars.peek() == Some(&ch) {
                    chars.next();
                    tokens.push(format!("{ch}{ch}"));
                } else {
                    tokens.push(ch.to_string());
                }
            }
            ';' | '\n' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(ch.to_string());
            }
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// The workflow half of the classifier reads command strings a workflow step
/// composed, so it has to see through the spellings a composed string reaches
/// for: `$'…'`, `${…}`, line continuations, comments, and a backtick used as
/// an escape. It also emits the grouping characters as their own tokens.
/// Both properties make it wrong for [`shell_tokens`]' callers; see the note
/// there.
pub(crate) fn workflow_shell_tokens(command: &str) -> Vec<String> {
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
        AnsiC,
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote = None;

    while let Some(ch) = chars.next() {
        if let Some(active) = quote {
            match active {
                Quote::Single => {
                    if ch == '\'' {
                        quote = None;
                    } else {
                        current.push(ch);
                    }
                }
                Quote::Double => {
                    if ch == '"' {
                        quote = None;
                    } else if ch == '\\' {
                        if let Some(next) = chars.next() {
                            if next == '\n' {
                                continue;
                            }
                            if next == '\r' && chars.peek() == Some(&'\n') {
                                chars.next();
                                continue;
                            }
                            if matches!(next, '"' | '\\' | '$' | '`') {
                                current.push(next);
                            } else {
                                current.push('\\');
                                current.push(next);
                            }
                        }
                    } else {
                        current.push(ch);
                    }
                }
                Quote::AnsiC => {
                    if ch == '\'' {
                        quote = None;
                    } else if ch == '\\' {
                        workflow_push_ansi_c_escape(&mut chars, &mut current);
                    } else {
                        current.push(ch);
                    }
                }
            }
            continue;
        }

        match ch {
            '$' if chars.peek() == Some(&'{') => {
                current.push('$');
                current.push(chars.next().expect("peeked parameter expansion"));
                let mut depth = 1;
                for next in chars.by_ref() {
                    current.push(next);
                    match next {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                quote = Some(Quote::AnsiC);
            }
            '\'' => quote = Some(Quote::Single),
            '"' => quote = Some(Quote::Double),
            '\\' => {
                if let Some(next) = chars.next() {
                    if next == '\n' {
                        continue;
                    }
                    if next == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                        continue;
                    }
                    if next.is_whitespace()
                        || matches!(
                            next,
                            '\'' | '"' | '\\' | '&' | '|' | ';' | '(' | ')' | '{' | '}'
                        )
                    {
                        current.push(next);
                    } else {
                        current.push('\\');
                        current.push(next);
                    }
                } else {
                    current.push('\\');
                }
            }
            '`' => {
                if let Some(next) = chars.next() {
                    if next == '\n' {
                        continue;
                    }
                    if next == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                        continue;
                    }
                    current.push(next);
                }
            }
            '#' if current.is_empty() => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        tokens.push("\n".to_owned());
                        break;
                    }
                }
            }
            '&' | '|' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                if ch == '|' && chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push("|&".to_owned());
                } else if chars.peek() == Some(&ch) {
                    chars.next();
                    tokens.push(format!("{ch}{ch}"));
                } else {
                    tokens.push(ch.to_string());
                }
            }
            ',' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(",".to_owned());
            }
            '{' if chars.peek() == Some(&'}') => {
                chars.next();
                current.push_str("{}");
            }
            ';' | '\n' | '(' | ')' | '{' | '}' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(ch.to_string());
            }
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

pub(crate) fn workflow_push_ansi_c_escape<I>(
    chars: &mut std::iter::Peekable<I>,
    output: &mut String,
) where
    I: Iterator<Item = char>,
{
    let Some(escape) = chars.next() else {
        output.push('\\');
        return;
    };
    match escape {
        '\n' => {}
        '\r' if chars.peek() == Some(&'\n') => {
            chars.next();
        }
        'a' => output.push('\u{7}'),
        'b' => output.push('\u{8}'),
        'e' | 'E' => output.push('\u{1b}'),
        'f' => output.push('\u{c}'),
        'n' => output.push('\n'),
        'r' => output.push('\r'),
        't' => output.push('\t'),
        'v' => output.push('\u{b}'),
        '\\' | '\'' | '"' | '?' => output.push(escape),
        'x' => workflow_push_radix_escape(chars, output, 16, 2),
        'u' => workflow_push_radix_escape(chars, output, 16, 4),
        'U' => workflow_push_radix_escape(chars, output, 16, 8),
        '0'..='7' => {
            let mut digits = String::from(escape);
            while digits.len() < 3 && chars.peek().is_some_and(|ch| matches!(ch, '0'..='7')) {
                digits.push(chars.next().expect("peeked octal digit"));
            }
            if let Ok(value) = u32::from_str_radix(&digits, 8) {
                if let Some(decoded) = char::from_u32(value) {
                    output.push(decoded);
                }
            }
        }
        _ => {
            output.push('\\');
            output.push(escape);
        }
    }
}

pub(crate) fn workflow_push_radix_escape<I>(
    chars: &mut std::iter::Peekable<I>,
    output: &mut String,
    radix: u32,
    max_digits: usize,
) where
    I: Iterator<Item = char>,
{
    let mut digits = String::new();
    while digits.len() < max_digits && chars.peek().is_some_and(|ch| ch.is_digit(radix)) {
        digits.push(chars.next().expect("peeked radix digit"));
    }
    if let Ok(value) = u32::from_str_radix(&digits, radix) {
        if let Some(decoded) = char::from_u32(value) {
            output.push(decoded);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is a verdict the three scanners produced before they were one
    /// scanner, recorded so a later change to [`Grammar`] cannot quietly move
    /// one of them. The interesting columns are where the three disagree:
    ///
    /// * `"a\\b"` is `ab` to the rule matchers and `a\\b` to the path
    ///   resolver. The rule matchers have always read a backslash inside double
    ///   quotes as escaping whatever follows; POSIX does not, and the resolver
    ///   needs POSIX because `"C:\\temp"` has to stay a path.
    /// * `*`, `~`, `{`, `#` and a bare `$` are ordinary characters to the rule
    ///   matchers — a rule matches the word a user wrote — and stop the path
    ///   resolver, which would otherwise treat a glob as a filename.
    /// * `&&` chains for the rule matcher, which checks each link; it is a
    ///   metacharacter to the other two.
    /// * An unterminated quote is not a metacharacter: the prefix editor asks
    ///   this question about a string the user is still typing.
    /// * A non-breaking space stays inside a word for the rule matchers, so
    ///   `echo\u{a0}rm` matches no `echo` rule.
    const PINNED: &[(&str, &str, bool, &str)] = &[
        ("git status", "Simple([\"git\", \"status\"])", false, "Some([\"git\", \"status\"])"),
        ("cargo test -p a && cargo test -p b", "SafeAndChain([[\"cargo\", \"test\", \"-p\", \"a\"], [\"cargo\", \"test\", \"-p\", \"b\"]])", true, "None"),
        ("ls | grep x", "UnsafeComplex", true, "None"),
        ("ls & git status", "UnsafeComplex", true, "None"),
        ("echo $(whoami)", "UnsafeComplex", true, "None"),
        ("echo `whoami`", "UnsafeComplex", true, "None"),
        ("cat < in.txt", "UnsafeComplex", true, "None"),
        ("(cd /tmp && ls)", "UnsafeComplex", true, "None"),
        ("echo \"a\\b\"", "Simple([\"echo\", \"ab\"])", false, "Some([\"echo\", \"a\\\\b\"])"),
        ("echo \"C:\\temp\\notes.txt\"", "Simple([\"echo\", \"C:tempnotes.txt\"])", false, "Some([\"echo\", \"C:\\\\temp\\\\notes.txt\"])"),
        ("mkdir -p build", "Simple([\"mkdir\", \"-p\", \"build\"])", false, "Some([\"mkdir\", \"-p\", \"build\"])"),
        ("mkdir -p \"C:\\Program Files\\x\"", "Simple([\"mkdir\", \"-p\", \"C:Program Filesx\"])", false, "Some([\"mkdir\", \"-p\", \"C:\\\\Program Files\\\\x\"])"),
        ("rm -rf *.tmp", "Simple([\"rm\", \"-rf\", \"*.tmp\"])", false, "None"),
        ("rm -rf ~/x", "Simple([\"rm\", \"-rf\", \"~/x\"])", false, "None"),
        ("rm -rf {a,b}", "Simple([\"rm\", \"-rf\", \"{a,b}\"])", false, "None"),
        ("echo hi # comment", "Simple([\"echo\", \"hi\", \"#\", \"comment\"])", false, "None"),
        ("echo $HOME", "Simple([\"echo\", \"$HOME\"])", false, "None"),
        ("echo \\\\?", "Simple([\"echo\", \"\\\\?\"])", false, "None"),
        ("echo a\\ b", "Simple([\"echo\", \"a b\"])", false, "Some([\"echo\", \"a b\"])"),
        ("echo \"\"", "Simple([\"echo\"])", false, "Some([\"echo\", \"\"])"),
        ("echo ''", "Simple([\"echo\"])", false, "Some([\"echo\", \"\"])"),
        ("echo \"unterminated", "UnsafeComplex", false, "None"),
        ("echo trailing\\", "UnsafeComplex", false, "None"),
        ("&& ls", "UnsafeComplex", true, "None"),
        ("ls &&", "UnsafeComplex", true, "None"),
        ("echo a\\\nb", "Simple([\"echo\", \"a\\nb\"])", false, "None"),
        ("echo one\ntwo", "UnsafeComplex", true, "None"),
        ("echo a\u{a0}b", "Simple([\"echo\", \"a\\u{a0}b\"])", false, "Some([\"echo\", \"a\", \"b\"])"),
    ];

    #[test]
    fn the_three_grammars_still_answer_what_they_answered_before_c10() {
        for (command, shape, metacharacter, static_argv) in PINNED {
            assert_eq!(
                &format!("{:?}", parse_bash_shape(command)),
                shape,
                "parse_bash_shape({command:?})"
            );
            assert_eq!(
                &matches!(
                    simple_command_segments(command, &ANY_METACHARACTER),
                    Err(Bail::Metacharacter)
                ),
                metacharacter,
                "has_unquoted_shell_metacharacter({command:?})"
            );
            let resolved = simple_command_segments(command, &STATIC_ARGV)
                .ok()
                .map(|mut segments| segments.remove(0));
            assert_eq!(
                &format!("{resolved:?}"),
                static_argv,
                "static argv of {command:?}"
            );
        }
    }

    /// The classifier's two token streams are not interchangeable, and the
    /// reason is not cosmetic: `workflow_shell_tokens` emits `(`, `)` and `.`
    /// boundaries that split a PowerShell method call apart, and the deletion
    /// detector reads that call whole.
    #[test]
    fn the_two_token_streams_disagree_where_powershell_needs_them_to() {
        let call = r#"[System.IO.FileInfo]::new("old.txt").Delete()"#;
        assert_eq!(
            shell_tokens(call),
            vec![r#"[System.IO.FileInfo]::new(old.txt).Delete()"#.to_string()],
            "the plain split keeps a method call in one word"
        );
        assert!(
            workflow_shell_tokens(call).len() > 1,
            "the workflow split breaks it apart, which is why it is not used here"
        );
    }
}
