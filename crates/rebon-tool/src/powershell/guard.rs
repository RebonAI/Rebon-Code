//! Pre-execution guards for the PowerShell tool.
//!
//! Two refusals, both about things the *approval* step cannot see:
//!
//! * [`hidden_character_refusal`] — a command carrying characters the
//!   permission prompt will not render is a command the user cannot actually
//!   review. An ANSI escape can erase the line it sits on and a bidi override
//!   can reverse it, so what the user approves and what runs are different
//!   strings. Refusing is the only honest answer.
//! * [`blocked_sleep_refusal`] — a long `Start-Sleep` burns the whole timeout
//!   window doing nothing, and the model almost always meant "wait for X".
//!   Point it at the mechanisms that actually wait.

/// Threshold: a sleep at or past this is a spin-wait, not a pause.
pub const MAX_INLINE_SLEEP_MS: u64 = 25_000;

/// Refuse a command whose text the approval prompt cannot faithfully show.
///
/// Newlines and tabs are allowed: here-strings are the documented way to pass
/// multi-line text to PowerShell (see [`super::prompt`]), and the prompt
/// renders them. Everything else in C0, plus the zero-width and
/// bidi-control ranges, is rejected.
pub fn hidden_character_refusal(command: &str) -> Option<String> {
    let (index, offender) = command
        .char_indices()
        .find(|(_, ch)| is_hidden_character(*ch))?;
    Some(format!(
        "The command contains a character the approval prompt cannot display \
         (U+{:04X} at byte offset {index}), so it cannot be reviewed before it runs. \
         Rewrite the command without escape sequences, zero-width, or bidirectional \
         control characters.",
        offender as u32
    ))
}

fn is_hidden_character(ch: char) -> bool {
    match ch {
        '\n' | '\t' | '\r' => false,
        // C0 controls (including ESC, which starts every ANSI sequence) and DEL.
        c if (c as u32) < 0x20 || c as u32 == 0x7F => true,
        // Soft hyphen and the Mongolian vowel separator: invisible, and both
        // split an identifier without showing it.
        '\u{00AD}' | '\u{180E}' => true,
        // Zero-width space/non-joiner/joiner and the LRM/RLM marks.
        '\u{200B}'..='\u{200F}' => true,
        // Bidi embedding, override, and pop — the line-reversal family.
        '\u{202A}'..='\u{202E}' => true,
        // Word joiner and the invisible math operators.
        '\u{2060}'..='\u{2064}' => true,
        // Bidi isolates.
        '\u{2066}'..='\u{2069}' => true,
        // Zero-width no-break space / BOM.
        '\u{FEFF}' => true,
        _ => false,
    }
}

/// Refuse a command that parks on `Start-Sleep` for [`MAX_INLINE_SLEEP_MS`] or
/// longer. Returns the refusal text, or `None` to let it through.
pub fn blocked_sleep_refusal(command: &str) -> Option<String> {
    let millis = longest_sleep_ms(command)?;
    (millis >= MAX_INLINE_SLEEP_MS).then(|| {
        format!(
            "This command sleeps for {:.0}s, which would hold the tool call open doing nothing. \
             Use the Monitor tool to wait on a condition, or `run_in_background: true` plus \
             ShellOutput to wait on the process itself.",
            millis as f64 / 1000.0
        )
    })
}

/// The longest `Start-Sleep` duration the command asks for, in milliseconds.
///
/// Deliberately syntactic: it walks tokens rather than resolving variables,
/// so `Start-Sleep $wait` is invisible to it. That is the right trade — the
/// guard exists to catch a model spelling out a long wait, not to prove a
/// command never sleeps.
pub fn longest_sleep_ms(command: &str) -> Option<u64> {
    let tokens = tokenize(command);
    let mut longest: Option<u64> = None;

    let mut index = 0usize;
    while index < tokens.len() {
        if !is_sleep_command(&tokens[index]) {
            index += 1;
            continue;
        }
        let mut scale_ms = 1000u64; // `Start-Sleep 5` means five seconds.
        let mut duration: Option<u64> = None;
        let mut cursor = index + 1;
        // Arguments to one `Start-Sleep` call: stop at the next statement.
        while cursor < tokens.len() && !is_statement_break(&tokens[cursor]) {
            let token = &tokens[cursor];
            match parameter_name(token).as_deref() {
                Some("milliseconds") | Some("ms") => scale_ms = 1,
                Some("seconds") | Some("s") => scale_ms = 1000,
                Some(_) => {}
                None => {
                    if let Some(value) = numeric_value(token) {
                        duration = Some(duration.unwrap_or(0).max(value));
                    }
                }
            }
            cursor += 1;
        }
        if let Some(value) = duration {
            let millis = value.saturating_mul(scale_ms);
            longest = Some(longest.unwrap_or(0).max(millis));
        }
        index = cursor.max(index + 1);
    }
    longest
}

fn is_sleep_command(token: &str) -> bool {
    let lowered = token.trim_matches(['\'', '"']).to_ascii_lowercase();
    matches!(lowered.as_str(), "start-sleep" | "sleep")
}

/// `;`, a pipe, or a chain operator ends the argument list of a cmdlet.
fn is_statement_break(token: &str) -> bool {
    matches!(token, ";" | "|" | "&&" | "||" | "{" | "}" | ")")
}

/// `-Seconds` → `Some("seconds")`; a non-parameter token → `None`.
///
/// A negative number (`-5`) is a value, not a parameter name.
fn parameter_name(token: &str) -> Option<String> {
    let rest = token.strip_prefix('-')?;
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .collect();
    (!name.is_empty() && name.len() == rest.len()).then(|| name.to_ascii_lowercase())
}

/// Parse a literal duration, including the `(60 * 5)` form the model reaches
/// for when it wants minutes.
fn numeric_value(token: &str) -> Option<u64> {
    let cleaned = token.trim_matches(['(', ')', '\'', '"']).trim();
    if cleaned.is_empty() {
        return None;
    }
    let mut product: u64 = 1;
    let mut saw_factor = false;
    for factor in cleaned.split('*') {
        let factor = factor.trim();
        let parsed: f64 = factor.parse().ok()?;
        if !parsed.is_finite() || parsed < 0.0 {
            return None;
        }
        product = product.saturating_mul(parsed.round() as u64);
        saw_factor = true;
    }
    saw_factor.then_some(product)
}

/// Split on whitespace while keeping `;` `|` `&&` `||` `{` `}` `)` as their
/// own tokens, so an argument list ends where the statement does.
fn tokenize(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            ';' | '{' | '}' | ')' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(ch.to_string());
            }
            '|' | '&' => {
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
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_commands_pass_the_hidden_character_check() {
        for command in [
            "Get-Process",
            "git commit -m @'\nline one\nline two\n'@",
            "Write-Output \"成功\"",
            "cargo test\t--all",
        ] {
            assert_eq!(hidden_character_refusal(command), None, "{command}");
        }
    }

    #[test]
    fn escape_and_bidi_characters_are_refused() {
        for command in [
            "Write-Output \u{1b}[2K harmless",
            "Remove-Item \u{202e}txt.exe",
            "Get-Process\u{200b}Evil",
            "Get-Content \u{feff}file",
        ] {
            let refusal = hidden_character_refusal(command)
                .unwrap_or_else(|| panic!("{command:?} should be refused"));
            assert!(refusal.contains("approval prompt"), "{refusal}");
        }
    }

    #[test]
    fn sleep_durations_are_read_from_every_argument_form() {
        assert_eq!(longest_sleep_ms("Start-Sleep 30"), Some(30_000));
        assert_eq!(longest_sleep_ms("Start-Sleep -Seconds 30"), Some(30_000));
        assert_eq!(longest_sleep_ms("start-sleep -s 45"), Some(45_000));
        assert_eq!(
            longest_sleep_ms("Start-Sleep -Milliseconds 30000"),
            Some(30_000)
        );
        assert_eq!(longest_sleep_ms("Start-Sleep -ms 250"), Some(250));
        assert_eq!(
            longest_sleep_ms("Start-Sleep -Seconds (60*5)"),
            Some(300_000)
        );
        assert_eq!(longest_sleep_ms("sleep 2"), Some(2_000));
    }

    #[test]
    fn only_the_arguments_of_the_sleep_itself_are_read() {
        // The `120` belongs to Wait-Process, not to Start-Sleep.
        assert_eq!(
            longest_sleep_ms("Start-Sleep -Seconds 1; Wait-Process -Timeout 120"),
            Some(1_000)
        );
        assert_eq!(
            longest_sleep_ms("Get-Process | Select-Object -First 5"),
            None
        );
        assert_eq!(longest_sleep_ms("Start-Sleep $wait"), None);
    }

    #[test]
    fn the_longest_sleep_in_a_compound_command_decides() {
        assert_eq!(
            longest_sleep_ms("Start-Sleep 1; Start-Sleep -Seconds 90; Write-Output done"),
            Some(90_000)
        );
    }

    #[test]
    fn refusal_fires_at_the_threshold_and_not_below_it() {
        assert!(blocked_sleep_refusal("Start-Sleep -Seconds 24").is_none());
        assert!(blocked_sleep_refusal("Start-Sleep -Milliseconds 24999").is_none());

        let refusal = blocked_sleep_refusal("Start-Sleep -Seconds 25").unwrap();
        assert!(refusal.contains("Monitor"), "{refusal}");
        assert!(refusal.contains("run_in_background"), "{refusal}");
        assert!(refusal.contains("25s"), "{refusal}");
    }
}
