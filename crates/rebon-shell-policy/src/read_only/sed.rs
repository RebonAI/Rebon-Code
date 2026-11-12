//! `sed` as a reader: `sed -n '10,40p' f`, `sed 's/a/b/' f`.
//!
//! sed writes through its options (`-i`, and `-f` hands it a script nobody
//! has read) and through its script (`w`, `W`, the `w` and `e` flags of `s`,
//! and `e`, which runs a command). So this reads the script itself, against
//! a grammar small enough to check by eye: addresses, the printing and
//! buffer commands, `{ }`, and `s` with flags that only print or match.
//! Anything outside it, including what it cannot parse, is not a read.
//!
//! GNU and BSD sed disagree on one thing that decides where a regex ends:
//! BSD skips a bracket expression whole, GNU does not, so `[/]` ends a
//! regex for one and not the other. A bracket expression that holds the
//! delimiter or a backslash is refused, which leaves both reading every
//! accepted script the same way.

/// Whether sed, given `args`, only prints.
pub(super) fn sed_args_only_read(args: &[String]) -> bool {
    let mut scripts: Vec<&str> = Vec::new();
    let mut operands: Vec<&str> = Vec::new();
    let mut options_done = false;
    let mut words = args.iter();
    while let Some(arg) = words.next() {
        let arg = arg.as_str();
        if options_done || arg == "-" || !arg.starts_with('-') {
            operands.push(arg);
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some(("expression", script)) => scripts.push(script),
                Some(_) => return false,
                None => match long {
                    "expression" => match words.next() {
                        Some(script) => scripts.push(script),
                        None => return false,
                    },
                    "quiet" | "silent" | "regexp-extended" | "unbuffered" | "null-data"
                    | "separate" | "posix" | "sandbox" | "debug" => {}
                    _ => return false,
                },
            }
            continue;
        }
        // A cluster of short flags; `e` takes the rest of the word, or the
        // next word, as a script.
        let cluster = &arg[1..];
        for (at, flag) in cluster.char_indices() {
            match flag {
                'n' | 'E' | 'r' | 's' | 'u' | 'z' => {}
                'e' => {
                    let rest = &cluster[at + 1..];
                    if !rest.is_empty() {
                        scripts.push(rest);
                    } else {
                        match words.next() {
                            Some(script) => scripts.push(script),
                            None => return false,
                        }
                    }
                    break;
                }
                _ => return false,
            }
        }
    }
    if scripts.is_empty() {
        let Some((script, _files)) = operands.split_first() else {
            return false;
        };
        scripts.push(script);
    }
    scripts.into_iter().all(script_only_reads)
}

/// Commands that print, move through the input, or shuffle the pattern and
/// hold spaces, and take no argument.
const PLAIN_COMMANDS: &str = "pP=dDnNgGhHx";

/// Delimiters accepted for `s`: the punctuation people use. Not `\`, a
/// bracket, a blank or a newline, whose meaning as a delimiter the two
/// seds do not agree on.
const S_DELIMITERS: &str = "/|#,:@!%+_~=";

/// `s` flags that print or change how the pattern matches. `w` writes a
/// file and `e` runs the pattern space, so neither is here.
const S_FLAGS: &str = "gpiImM0123456789";

fn script_only_reads(script: &str) -> bool {
    let chars: Vec<char> = script.chars().collect();
    let mut at = 0;
    loop {
        skip(&chars, &mut at, |ch| matches!(ch, ' ' | '\t' | '\n' | ';'));
        let Some(&first) = chars.get(at) else {
            return true;
        };
        if first == '}' {
            at += 1;
            if !command_ends(&chars, &mut at) {
                return false;
            }
            continue;
        }
        if !address(&chars, &mut at) {
            return false;
        }
        skip(&chars, &mut at, |ch| matches!(ch, ' ' | '\t'));
        if chars.get(at) == Some(&',') {
            at += 1;
            if matches!(chars.get(at), Some('+' | '~')) {
                at += 1;
                if !digits(&chars, &mut at) {
                    return false;
                }
            } else {
                // The second address is not optional.
                let start = at;
                if !address(&chars, &mut at) || at == start {
                    return false;
                }
            }
        }
        skip(&chars, &mut at, |ch| matches!(ch, ' ' | '\t'));
        if chars.get(at) == Some(&'!') {
            at += 1;
            skip(&chars, &mut at, |ch| matches!(ch, ' ' | '\t'));
        }
        let Some(&command) = chars.get(at) else {
            return false;
        };
        at += 1;
        match command {
            '{' => continue,
            command if PLAIN_COMMANDS.contains(command) => {}
            // An exit code, or a line length.
            'q' | 'Q' | 'l' => {
                skip(&chars, &mut at, |ch| matches!(ch, ' ' | '\t'));
                digits(&chars, &mut at);
            }
            's' => {
                let Some(&delimiter) = chars.get(at) else {
                    return false;
                };
                if !S_DELIMITERS.contains(delimiter) {
                    return false;
                }
                at += 1;
                if !regex_body(&chars, &mut at, delimiter)
                    || !replacement_body(&chars, &mut at, delimiter)
                {
                    return false;
                }
                skip(&chars, &mut at, |ch| S_FLAGS.contains(ch));
            }
            _ => return false,
        }
        if !command_ends(&chars, &mut at) {
            return false;
        }
    }
}

/// An optional address: a line number (or GNU `first~step`), `$`, or
/// `/regex/` with GNU's `I` / `M` flags. Absent is fine.
fn address(chars: &[char], at: &mut usize) -> bool {
    match chars.get(*at) {
        Some('$') => {
            *at += 1;
            true
        }
        Some('/') => {
            *at += 1;
            if !regex_body(chars, at, '/') {
                return false;
            }
            skip(chars, at, |ch| matches!(ch, 'I' | 'M'));
            true
        }
        Some(ch) if ch.is_ascii_digit() => {
            digits(chars, at);
            if chars.get(*at) == Some(&'~') {
                *at += 1;
                return digits(chars, at);
            }
            true
        }
        _ => true,
    }
}

/// A regex up to its closing `delimiter`, which is consumed. A backslash
/// escapes the next character; a bracket expression is skipped whole and
/// refused if it holds the delimiter, a backslash or a newline (see the
/// module note).
fn regex_body(chars: &[char], at: &mut usize, delimiter: char) -> bool {
    while let Some(&ch) = chars.get(*at) {
        *at += 1;
        match ch {
            '\n' => return false,
            '\\' => {
                if chars.get(*at).is_none_or(|next| *next == '\n') {
                    return false;
                }
                *at += 1;
            }
            '[' => {
                if !bracket_expression(chars, at, delimiter) {
                    return false;
                }
            }
            ch if ch == delimiter => return true,
            _ => {}
        }
    }
    false
}

/// The rest of a bracket expression after its `[`, up to and including the
/// closing `]`. A `]` first (after an optional `^`) is literal; `[:x:]`,
/// `[.x.]` and `[=x=]` are skipped to their own close.
fn bracket_expression(chars: &[char], at: &mut usize, delimiter: char) -> bool {
    let refused = |ch: char| ch == delimiter || ch == '\\' || ch == '\n';
    if chars.get(*at) == Some(&'^') {
        *at += 1;
    }
    if chars.get(*at) == Some(&']') {
        *at += 1;
    }
    while let Some(&ch) = chars.get(*at) {
        *at += 1;
        if refused(ch) {
            return false;
        }
        match ch {
            ']' => return true,
            '[' if matches!(chars.get(*at), Some(':' | '.' | '=')) => {
                let kind = chars[*at];
                *at += 1;
                loop {
                    let Some(&inner) = chars.get(*at) else {
                        return false;
                    };
                    *at += 1;
                    if refused(inner) {
                        return false;
                    }
                    if inner == kind && chars.get(*at) == Some(&']') {
                        *at += 1;
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// An `s` replacement up to its closing `delimiter`, which is consumed.
fn replacement_body(chars: &[char], at: &mut usize, delimiter: char) -> bool {
    while let Some(&ch) = chars.get(*at) {
        *at += 1;
        match ch {
            '\n' => return false,
            '\\' => {
                if chars.get(*at).is_none() {
                    return false;
                }
                *at += 1;
            }
            ch if ch == delimiter => return true,
            _ => {}
        }
    }
    false
}

/// After a command: blanks, then the end, `;`, a newline or `}`.
fn command_ends(chars: &[char], at: &mut usize) -> bool {
    skip(chars, at, |ch| matches!(ch, ' ' | '\t'));
    matches!(chars.get(*at), None | Some(';' | '\n' | '}'))
}

fn digits(chars: &[char], at: &mut usize) -> bool {
    let start = *at;
    skip(chars, at, |ch| ch.is_ascii_digit());
    *at > start
}

fn skip(chars: &[char], at: &mut usize, keep: impl Fn(char) -> bool) {
    while chars.get(*at).is_some_and(|ch| keep(*ch)) {
        *at += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads(args: &[&str]) -> bool {
        sed_args_only_read(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn printing_scripts_are_reads() {
        for args in [
            &["-n", "10,40p", "src/lib.rs"][..],
            &["-n", "1,$p", "f"],
            &["-n", "$p", "f"],
            &["-n", "5p;20p", "f"],
            &["-n", "/^fn main/,/^}/p", "f"],
            &["-n", "/^\\[package\\]/,/^\\[/p", "Cargo.toml"],
            &["-n", "/[0-9][[:space:]]/p", "f"],
            &["-n", "/foo/Ip", "f"],
            &["-n", "/start/,+5p", "f"],
            &["-n", "0~4p", "f"],
            &["-n", "/a/!p", "f"],
            &["-n", "/a/{p;=}", "f"],
            &["-n", "-e", "1p", "-e", "$p", "f"],
            &["-ne", "1,3p", "f"],
            &["-nE", "/a|b/p", "f"],
            &["--quiet", "--expression=2p", "f"],
            &["10q", "f"],
            &["5d", "f"],
            &["s/foo/bar/g", "f"],
            &["-n", "s|a/b|c|p", "f"],
            &["s#\\##x#2", "f"],
            &["-E", "s/([a-z]+)/<\\1>/", "f"],
            &["1!G;h;$!d", "f"],
            &["-n", "l 80", "f"],
        ] {
            assert!(reads(args), "expected {args:?} to read");
        }
    }

    #[test]
    fn writing_running_or_unreadable_scripts_are_not() {
        for args in [
            // In-place, a script file, or no script.
            &["-i", "s/a/b/", "f"][..],
            &["-i.bak", "s/a/b/", "f"],
            &["-ni", "p", "f"],
            &["-I", "", "s/a/b/", "f"],
            &["--in-place", "s/a/b/", "f"],
            &["--in-place=.bak", "s/a/b/", "f"],
            &["-f", "script.sed", "f"],
            &["--file=script.sed", "f"],
            &["-n"],
            &[],
            &["-e"],
            // Commands that write, run or read another file.
            &["-n", "w out", "f"],
            &["-n", "1,5w out", "f"],
            &["W out", "f"],
            &["e ls", "f"],
            &["1e", "f"],
            &["r other", "f"],
            &["R other", "f"],
            &["s/a/b/w out", "f"],
            &["s/a/b/gw out", "f"],
            &["s/a/b/e", "f"],
            &["-n", "p;w out", "f"],
            &["-n", "/a/{p;w out}", "f"],
            // Text and branching commands are outside the grammar.
            &["a\\text", "f"],
            &["i text", "f"],
            &["c text", "f"],
            &[":a;N;ba", "f"],
            &["# comment", "f"],
            // Delimiters and brackets the two seds read differently.
            &["s\\a\\b\\", "f"],
            &["-n", "\\,a,p", "f"],
            &["-n", "/[/]w x/p", "f"],
            &["s/[/]/x/w out", "f"],
            &["-n", "/[\\/]/p", "f"],
            &["s a b ", "f"],
            // Unterminated.
            &["-n", "/abc", "f"],
            &["s/a/b", "f"],
            &["-n", "/[abc/p", "f"],
            &["-n", "1,p", "f"],
            &["-n", "1~p", "f"],
            &["-n", "p x", "f"],
        ] {
            assert!(!reads(args), "expected {args:?} to be refused");
        }
    }
}
