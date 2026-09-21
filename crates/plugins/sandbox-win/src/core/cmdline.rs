//! Rebuilding a Windows command line from an argv.
//!
//! `sandbox-win exec` receives the confined process's argv after `--` and has
//! to hand it to `CreateProcessWithLogonW`, which takes a single string. That
//! conversion is the inverse of `CommandLineToArgvW`, and it is the one place
//! in the helper where naive string concatenation is a security bug rather
//! than a cosmetic one.
//!
//! The argv travels untouched for the sake of the PowerShell path:
//! `pwsh -NoProfile -NonInteractive -EncodedCommand <base64>` arrives as five
//! arguments, so no layer between Rebon and the shell ever parses the payload
//! and a `"` inside it has nothing to break. That property survives exactly as
//! long as this module re-quotes correctly.
//!
//! [`parse_command_line`] is the reference implementation of
//! `CommandLineToArgvW` in the other direction, so the round trip can be
//! asserted on any host; the same property is re-checked against the real Win32
//! function in [`crate::sys::cmdline`], which is the only place it can be.

/// Quote one argument the way `CommandLineToArgvW` will read back.
///
/// The backslash rule is the subtle half: a run of backslashes is literal
/// unless it precedes a quote, in which case each one must be doubled. Get it
/// wrong and a trailing `C:\dir\` escapes the closing quote, swallowing the
/// next argument into this one.
pub fn quote_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_string();
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                // The backslashes precede a quote, so each becomes two, and the quote itself
                // needs one more.
                for _ in 0..backslashes * 2 + 1 {
                    quoted.push('\\');
                }
                backslashes = 0;
                quoted.push('"');
            }
            _ => {
                for _ in 0..backslashes {
                    quoted.push('\\');
                }
                backslashes = 0;
                quoted.push(character);
            }
        }
    }
    // A run at the end precedes the closing quote, so it is doubled too.
    for _ in 0..backslashes * 2 {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

/// Join an argv into the single string `CreateProcessW` takes.
pub fn build_command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| quote_argument(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `CommandLineToArgvW`, reimplemented.
///
/// Faithful to two quirks that matter for the round trip:
///
/// * **`argv[0]` has its own rules.** No backslash escaping at all: a leading
///   `"` runs to the next `"`, otherwise the argument ends at whitespace.
///   This is why a program path containing a quote can never round-trip — and
///   why Windows does not allow one.
/// * **`""` inside quotes is a literal quote.** This helper never emits that
///   form, but a command line arriving from elsewhere may contain it.
pub fn parse_command_line(command_line: &str) -> Vec<String> {
    let characters: Vec<char> = command_line.chars().collect();
    let mut index = 0usize;
    let mut argv = Vec::new();

    while index < characters.len() && characters[index].is_whitespace() {
        index += 1;
    }
    if index >= characters.len() {
        return argv;
    }

    // argv[0]
    let mut first = String::new();
    if characters[index] == '"' {
        index += 1;
        while index < characters.len() && characters[index] != '"' {
            first.push(characters[index]);
            index += 1;
        }
        if index < characters.len() {
            index += 1; // consume the closing quote
        }
    } else {
        while index < characters.len() && !characters[index].is_whitespace() {
            first.push(characters[index]);
            index += 1;
        }
    }
    argv.push(first);

    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;

    while index < characters.len() {
        let character = characters[index];

        if character == '\\' {
            let mut backslashes = 0usize;
            while index < characters.len() && characters[index] == '\\' {
                backslashes += 1;
                index += 1;
            }
            if index < characters.len() && characters[index] == '"' {
                for _ in 0..backslashes / 2 {
                    current.push('\\');
                }
                if backslashes % 2 == 1 {
                    current.push('"');
                    started = true;
                    index += 1;
                } else {
                    // An even run leaves the quote as a delimiter; the loop handles it on the
                    // next pass.
                }
            } else {
                for _ in 0..backslashes {
                    current.push('\\');
                }
                started = true;
            }
            continue;
        }

        if character == '"' {
            if in_quotes && index + 1 < characters.len() && characters[index + 1] == '"' {
                current.push('"');
                index += 2;
                started = true;
                continue;
            }
            in_quotes = !in_quotes;
            started = true;
            index += 1;
            continue;
        }

        if character.is_whitespace() && !in_quotes {
            if started {
                argv.push(std::mem::take(&mut current));
                started = false;
            }
            index += 1;
            continue;
        }

        current.push(character);
        started = true;
        index += 1;
    }

    if started {
        argv.push(current);
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip through the reference parser, skipping `argv[0]`'s legacy rules
    /// by putting a plain program name in front.
    fn round_trip(arguments: &[&str]) -> Vec<String> {
        let mut argv = vec!["prog.exe".to_string()];
        argv.extend(arguments.iter().map(|a| (*a).to_string()));
        let parsed = parse_command_line(&build_command_line(&argv));
        parsed.into_iter().skip(1).collect()
    }

    fn assert_round_trips(arguments: &[&str]) {
        let expected: Vec<String> = arguments.iter().map(|a| (*a).to_string()).collect();
        assert_eq!(round_trip(arguments), expected, "argv {arguments:?}");
    }

    #[test]
    fn a_plain_argument_is_not_quoted_at_all() {
        assert_eq!(quote_argument("echo"), "echo");
        assert_eq!(quote_argument(r"C:\work\build"), r"C:\work\build");
    }

    #[test]
    fn spaces_and_tabs_force_quoting() {
        assert_eq!(quote_argument("a b"), r#""a b""#);
        assert_eq!(quote_argument("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn an_empty_argument_survives_as_an_empty_argument() {
        // Without the quotes it would vanish, shifting every later argument one
        // position left.
        assert_eq!(quote_argument(""), r#""""#);
        assert_round_trips(&["", "after"]);
    }

    #[test]
    fn a_trailing_backslash_does_not_escape_the_closing_quote() {
        // The classic failure: `"C:\dir\"` reads as an unterminated argument and
        // swallows whatever follows it.
        assert_eq!(
            quote_argument(r"C:\dir with space\"),
            r#""C:\dir with space\\""#
        );
        assert_round_trips(&[r"C:\dir with space\", "next"]);
    }

    #[test]
    fn embedded_quotes_survive() {
        assert_round_trips(&[r#"say "hi""#]);
        assert_round_trips(&[r#"{"key": "value"}"#]);
    }

    #[test]
    fn backslashes_before_a_quote_are_doubled() {
        assert_eq!(quote_argument(r#"a\"b"#), r#""a\\\"b""#);
        assert_round_trips(&[r#"a\"b"#]);
    }

    #[test]
    fn a_run_of_backslashes_not_before_a_quote_stays_literal() {
        assert_round_trips(&[r"a\\\b"]);
        assert_round_trips(&[r"\\server\share\path"]);
    }

    #[test]
    fn the_encoded_command_shape_from_the_powershell_rfc_survives() {
        // This is the whole reason argv is passed through untouched: the base64
        // payload must arrive as one argument with nothing done to it.
        assert_round_trips(&[
            "pwsh.exe",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            "ZQBjAGgAbwAgACIAaABlAGwAbABvACIA",
        ]);
    }

    #[test]
    fn a_quoted_program_path_with_spaces_parses_as_argv_zero() {
        let argv = vec![
            r"C:\Program Files\Rebon\sandbox-win.exe".to_string(),
            "exec".into(),
        ];
        let parsed = parse_command_line(&build_command_line(&argv));
        assert_eq!(parsed, argv);
    }

    #[test]
    fn argv_zero_uses_the_legacy_rules() {
        // No backslash escaping in the first argument: that is Windows' behaviour
        // rather than ours, and the parser has to model it or the round trip test
        // would be checking the wrong function.
        assert_eq!(
            parse_command_line(r#""C:\a\b\" rest"#),
            vec![r"C:\a\b\".to_string(), "rest".to_string()]
        );
    }

    #[test]
    fn a_doubled_quote_inside_quotes_is_one_literal_quote() {
        assert_eq!(
            parse_command_line(r#"prog "a""b""#),
            vec!["prog".to_string(), r#"a"b"#.to_string()]
        );
    }

    #[test]
    fn an_empty_command_line_parses_to_nothing() {
        assert!(parse_command_line("").is_empty());
        assert!(parse_command_line("   ").is_empty());
    }

    #[test]
    fn runs_of_whitespace_do_not_create_empty_arguments() {
        assert_eq!(
            parse_command_line("prog   a    b"),
            vec!["prog".to_string(), "a".into(), "b".into()]
        );
    }

    #[test]
    fn every_argument_shape_round_trips() {
        // A deterministic sweep rather than a random one: the interesting space here
        // is small and enumerable, and a fixed matrix fails the same way on every
        // machine.
        let pieces = [
            "",
            "a",
            " ",
            "\t",
            "\"",
            "\\",
            "\\\\",
            "\\\"",
            "\"\"",
            "a b",
            "a\\b",
            "a\\\\b",
            "a\"b",
            "\\a",
            "a\\",
            "\\\\?\\C:\\x",
            "unicode-中文",
            "emoji-\u{1F600}",
        ];
        for first in pieces {
            for second in pieces {
                assert_round_trips(&[first, second]);
            }
        }
    }

    #[test]
    fn generated_arguments_round_trip() {
        // A pseudo-random pass over the same alphabet, longer and in more combinations
        // than the matrix above. Seeded, so a failure reproduces without a `rand`
        // dependency.
        let alphabet: Vec<char> = r#"ab \"#.chars().chain(['"', '\t']).collect();
        let mut state: u64 = 0x5EED_1234_ABCD_0001;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };
        for _ in 0..2000 {
            let count = 1 + next() % 3;
            let arguments: Vec<String> = (0..count)
                .map(|_| {
                    let length = next() % 8;
                    (0..length)
                        .map(|_| alphabet[next() % alphabet.len()])
                        .collect()
                })
                .collect();
            let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
            assert_round_trips(&borrowed);
        }
    }
}
