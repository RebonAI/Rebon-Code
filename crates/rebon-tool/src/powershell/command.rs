//! Command construction for the PowerShell tool.
//!
//! What actually reaches `-Command` is a sandwich:
//!
//! ```text
//! [utf8 prologue] + user command + [exit-code epilogue]
//! ```
//!
//! Both halves exist because `pwsh -Command <string>` on its own gets two
//! things wrong that the model cannot work around from inside the command:
//!
//! * **Encoding and colour.** Redirected output otherwise carries ANSI escape
//!   sequences (pwsh 7 renders colour even into a pipe) and `Out-File` writes
//!   UTF-16LE on 5.1. Both corrupt the tool result the model reads back.
//! * **Exit codes.** A cmdlet that fails still exits the process 0, so a
//!   failed command reads as a success. The epilogue folds the three sources
//!   of truth (`$LASTEXITCODE`, `$?`, neither) into one process exit code.

/// UTF-8 prologue prefix. Injected unless the command is a script-level
/// declaration.
///
/// Guarded three ways because it runs before anything the user asked for and
/// must never be the reason a command fails:
/// * every assignment is wrapped in `try`/`catch`, so a locked-down host that
///   rejects one still runs the command;
/// * the `$OutputEncoding`/`$PSStyle` half is skipped outside FullLanguage
///   mode, where writing those is itself a policy violation;
/// * `$PSStyle` is null-checked, since Windows PowerShell 5.1 has no such
///   variable.
const UTF8_PROLOGUE: &str =
    "try { $PSDefaultParameterValues['Out-File:Encoding'] = 'utf8' } catch {}\n\
if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') {\n\
try { $OutputEncoding = [System.Text.UTF8Encoding]::new() } catch {}\n\
if ($null -ne $PSStyle) { try { $PSStyle.OutputRendering = 'PlainText' } catch {} }\n\
}";

/// Pins PowerShell's own stdout encoding to the Windows console code page.
///
/// A native command that is the last element of a pipeline inherits our pipe
/// handle directly — its bytes never pass through PowerShell — so `taskkill /?`
/// on a CP936 box writes GBK no matter what PowerShell is configured to do.
/// PowerShell's *own* output, meanwhile, uses `[Console]::OutputEncoding`,
/// which pwsh 7 defaults to UTF-8. Left alone that produces one stream in two
/// encodings, and no decoder can undo that.
///
/// So the two are aligned on the code page the native tools already use, and
/// [`crate::shell_process::ShellOutputEncoding::powershell`] decodes with the
/// same page. Windows PowerShell 5.1 already behaves this way; pinning makes
/// it a guarantee instead of a default, and makes pwsh 7 match.
///
/// `$OutputEncoding` (the encoding used when piping text *into* a native
/// command) follows the same page for the same reason.
///
/// Skipped when the console page is already UTF-8 (65001): `GetEncoding(65001)`
/// returns a UTF8Encoding that emits a BOM, which would prepend U+FEFF to the
/// first line of output.
fn console_code_page_prologue(code_page: u32) -> Option<String> {
    if code_page == 65001 {
        return None;
    }
    Some(format!(
        "if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') {{\n\
         try {{ [Console]::OutputEncoding = [System.Text.Encoding]::GetEncoding({code_page}) }} catch {{}}\n\
         try {{ $OutputEncoding = [Console]::OutputEncoding }} catch {{}}\n\
         }}"
    ))
}

/// Exit-code epilogue suffix. Always injected.
///
/// `$?` is captured into a variable on the first line rather than read inside
/// the `if` condition: it reflects the *previous statement*, and any statement
/// between the user's command and the read can clobber it.
///
/// `$host.SetShouldExit` is what makes a non-zero code survive `-Command` in
/// FullLanguage mode; `exit` is the ConstrainedLanguage fallback, where
/// `SetShouldExit` is blocked.
const EXIT_CODE_EPILOGUE: &str = "$__rebon_ok = $?\n\
$__rebon_exit = if ($null -ne $LASTEXITCODE) { $LASTEXITCODE } elseif ($__rebon_ok) { 0 } else { 1 }\n\
if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') { $host.SetShouldExit($__rebon_exit) } else { exit $__rebon_exit }";

/// Spawn arguments for the runtime.
///
/// * `-NoProfile` — a user profile makes runs non-reproducible and can print
///   banners into stdout.
/// * `-NonInteractive` — interactive cmdlets fail fast instead of hanging on
///   a console that will never answer (the description warns about this too).
pub const SPAWN_FLAGS: [&str; 2] = ["-NoProfile", "-NonInteractive"];

/// Build the string handed to `-Command`.
///
/// `console_code_page` is `Some` on Windows — the page the reader will decode
/// with — and `None` everywhere else, where the whole stack is already UTF-8.
pub fn build_exec_command(command: &str, console_code_page: Option<u32>) -> String {
    // A script-level declaration has to stay the first thing in the
    // script, so it forgoes the prologue. The epilogue always applies — it is
    // appended, and nothing may follow it.
    if is_script_level_declaration(command) {
        return format!("{command}\n{EXIT_CODE_EPILOGUE}");
    }
    let mut prologue = UTF8_PROLOGUE.to_string();
    if let Some(console) = console_code_page.and_then(console_code_page_prologue) {
        prologue.push('\n');
        prologue.push_str(&console);
    }
    format!("{prologue}\n{command}\n{EXIT_CODE_EPILOGUE}")
}

/// Full argv for the runtime, ready to hand to `Command::args`.
pub fn spawn_args(command: &str, console_code_page: Option<u32>) -> Vec<String> {
    let mut args: Vec<String> = SPAWN_FLAGS.iter().map(|flag| (*flag).to_string()).collect();
    args.push("-Command".to_string());
    args.push(build_exec_command(command, console_code_page));
    args
}

/// The shell prefix for a *sandboxed* PowerShell command.
///
/// `-Command` is fine when Rebon spawns pwsh itself: the script is one
/// argv element and nothing re-parses it. Under a sandbox it is not,
/// because the script now travels through another layer first — a
/// `bwrap` argv, a seatbelt-wrapped `sh -lc`, or `sandbox-win`'s own
/// command line — and a script containing quotes, newlines, or `$(…)`
/// has to survive all of them unchanged.
///
/// `-EncodedCommand` sidesteps the whole problem: the payload becomes
/// base64 of UTF-16LE, which is `[A-Za-z0-9+/=]` and therefore has
/// nothing any of those layers can interpret. The cost is roughly
/// 2.7× the byte count, which is why the unsandboxed path does not
/// pay it and why Windows enforces a command-line length cap.
pub fn sandbox_spawn_prefix() -> Vec<String> {
    let mut args: Vec<String> = SPAWN_FLAGS.iter().map(|flag| (*flag).to_string()).collect();
    args.push("-EncodedCommand".to_string());
    args
}

/// Encode a script the way `-EncodedCommand` expects.
///
/// PowerShell decodes with `[System.Text.Encoding]::Unicode`, which
/// is UTF-16 **little-endian without a BOM** — not UTF-8, and not
/// UTF-16BE. Getting the endianness wrong does not error; it produces
/// a script of CJK-looking mojibake that fails with a parse error
/// pointing at nothing.
pub fn encode_command(script: &str) -> String {
    use base64::Engine as _;
    let utf16le: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(utf16le)
}

/// The encoded payload for `command`, prologue and epilogue included.
///
/// Same sandwich as [`build_exec_command`] — the exit-code epilogue in
/// particular is not optional, since a sandboxed command that exits 0
/// on failure is indistinguishable from one the sandbox blocked.
pub fn build_encoded_exec_command(command: &str, console_code_page: Option<u32>) -> String {
    encode_command(&build_exec_command(command, console_code_page))
}

/// Environment overrides, applied on top of the inherited environment.
///
/// `NO_COLOR` is skipped when the session already asked for colour with
/// `FORCE_COLOR` — a user who set that wants it, and silently overriding one
/// with the other produces output nobody asked for.
pub fn environment_overrides() -> Vec<(&'static str, &'static str)> {
    let mut overrides = vec![("PYTHONIOENCODING", "utf-8:surrogateescape")];
    if std::env::var_os("FORCE_COLOR").is_none() {
        overrides.push(("NO_COLOR", "1"));
    }
    overrides
}

/// Whether `command` opens with a construct that must be the first
/// thing in the script.
///
/// `using`, `param`, and the named script blocks are only legal at the very
/// top of a script; prepending the prologue in front of one turns a valid
/// command into a parse error. A leading type literal is included because
/// `[CmdletBinding()]`-style attributes sit above `param(...)` — but
/// `[Type]::StaticCall()` is an ordinary expression and does take the prologue.
pub fn is_script_level_declaration(command: &str) -> bool {
    let rest = skip_leading_trivia(command);
    let lowered = rest.to_ascii_lowercase();

    for keyword in ["using namespace", "using module", "using assembly"] {
        if lowered.starts_with(keyword) {
            return true;
        }
    }
    if starts_with_word_then(&lowered, "param", '(') {
        return true;
    }
    for keyword in ["begin", "process", "end", "clean", "dynamicparam"] {
        if starts_with_word_then(&lowered, keyword, '{') {
            return true;
        }
    }

    // A leading `[` is an attribute or type-constraint declaration unless the
    // closing bracket is followed by `::`, which makes it a static member
    // access — an expression, not a declaration.
    if let Some(after) = rest.strip_prefix('[') {
        if let Some(close) = after.find(']') {
            let tail = after[close + 1..].trim_start();
            return !tail.starts_with("::");
        }
    }
    false
}

/// Skip whitespace, `#` line comments, and `<# … #>` block comments.
fn skip_leading_trivia(command: &str) -> &str {
    let mut rest = command.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("<#") {
            match after.find("#>") {
                Some(end) => rest = after[end + 2..].trim_start(),
                // Unterminated block comment: the whole command is trivia, so
                // there is no declaration to protect.
                None => return "",
            }
            continue;
        }
        if rest.starts_with('#') {
            match rest.find('\n') {
                Some(end) => rest = rest[end + 1..].trim_start(),
                None => return "",
            }
            continue;
        }
        return rest;
    }
}

/// `word` followed (after optional whitespace) by `opener`, and `word` not
/// being a prefix of a longer identifier.
fn starts_with_word_then(lowered: &str, word: &str, opener: char) -> bool {
    let Some(rest) = lowered.strip_prefix(word) else {
        return false;
    };
    let trimmed = rest.trim_start();
    trimmed.starts_with(opener)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative non-Unicode Windows console page (Simplified Chinese).
    const GBK: Option<u32> = Some(936);

    // ── the sandboxed passing mode ────────────────────────────────

    fn decode_utf16le_base64(encoded: &str) -> String {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        assert_eq!(bytes.len() % 2, 0, "UTF-16 is a whole number of units");
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).expect("valid UTF-16")
    }

    #[test]
    fn encode_command_round_trips_through_utf16le() {
        for script in ["Get-Process", "Write-Output '你好'", "a\nb", "$x = \"q\""] {
            assert_eq!(decode_utf16le_base64(&encode_command(script)), script);
        }
    }

    #[test]
    fn encode_command_uses_little_endian_without_a_bom() {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encode_command("A"))
            .unwrap();
        // 'A' is U+0041; little-endian puts the low byte first, and a
        // BOM would have added two bytes in front.
        assert_eq!(bytes, vec![0x41, 0x00]);
    }

    #[test]
    fn encoded_payload_is_safe_for_every_wrapping_layer() {
        let hostile = "Write-Output 'a'; $(id) `x` \"b\"\n#c";
        let encoded = encode_command(hostile);
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "base64 must contain nothing a shell or command line can interpret: {encoded}"
        );
        assert_eq!(decode_utf16le_base64(&encoded), hostile);
    }

    #[test]
    fn the_sandbox_prefix_ends_with_encoded_command() {
        let prefix = sandbox_spawn_prefix();
        assert_eq!(prefix.last().map(String::as_str), Some("-EncodedCommand"));
        for flag in SPAWN_FLAGS {
            assert!(prefix.iter().any(|arg| arg == flag), "{flag} missing");
        }
    }

    #[test]
    fn the_unsandboxed_prefix_still_ends_with_command() {
        let args = spawn_args("Get-Process", None);
        assert_eq!(args[args.len() - 2], "-Command");
    }

    #[test]
    fn the_encoded_payload_carries_the_same_sandwich_as_the_plain_one() {
        for code_page in [None, GBK, Some(65001)] {
            let plain = build_exec_command("Get-Process", code_page);
            let decoded =
                decode_utf16le_base64(&build_encoded_exec_command("Get-Process", code_page));
            assert_eq!(decoded, plain);
            // The epilogue is what makes a failed command exit
            // non-zero. Under a sandbox, losing it would make a
            // blocked command look like a successful one.
            assert!(decoded.ends_with(EXIT_CODE_EPILOGUE));
        }
    }

    #[test]
    fn ordinary_command_is_wrapped_on_both_sides() {
        for code_page in [None, GBK, Some(65001)] {
            let built = build_exec_command("Get-Process", code_page);
            assert!(built.starts_with(UTF8_PROLOGUE), "{built}");
            assert!(built.ends_with(EXIT_CODE_EPILOGUE), "{built}");
            assert!(built.contains("\nGet-Process\n"), "{built}");
        }
    }

    /// PowerShell's own output is aligned onto the page its native children
    /// already write, so the stream carries one encoding rather than two.
    #[test]
    fn console_encoding_is_pinned_to_the_readers_code_page() {
        let built = build_exec_command("Get-Process", GBK);
        assert!(built.contains("[Console]::OutputEncoding"), "{built}");
        assert!(built.contains("GetEncoding(936)"), "{built}");
        assert!(built.contains("$OutputEncoding = [Console]::OutputEncoding"));
    }

    /// `GetEncoding(65001)` emits a BOM, which would land at the head of the
    /// first output line — and a UTF-8 console needs no alignment anyway.
    #[test]
    fn a_utf8_console_is_left_alone() {
        let built = build_exec_command("Get-Process", Some(65001));
        assert!(!built.contains("[Console]::OutputEncoding"), "{built}");
    }

    #[test]
    fn non_windows_gets_no_console_pin() {
        let built = build_exec_command("Get-Process", None);
        assert!(!built.contains("[Console]::OutputEncoding"), "{built}");
    }

    #[test]
    fn script_level_declarations_keep_their_first_line() {
        for command in [
            "using namespace System.IO\nGet-ChildItem",
            "param($Name)\nWrite-Output $Name",
            "  param ( $Name )",
            "[CmdletBinding()]\nparam($Name)",
            "process { $_ }",
            "# leading comment\nparam($Name)",
            "<# block #> using module Foo",
        ] {
            let built = build_exec_command(command, GBK);
            assert!(
                !built.starts_with(UTF8_PROLOGUE),
                "prologue must not precede {command:?}"
            );
            assert!(built.starts_with(command), "{built}");
            assert!(built.ends_with(EXIT_CODE_EPILOGUE), "{built}");
        }
    }

    #[test]
    fn static_member_access_is_an_expression_not_a_declaration() {
        for command in [
            "[System.Environment]::SystemDirectory",
            "[Console]::Error.WriteLine('x')",
            "[int]::MaxValue",
        ] {
            assert!(
                !is_script_level_declaration(command),
                "{command} should take the prologue"
            );
            assert!(build_exec_command(command, GBK).starts_with(UTF8_PROLOGUE));
        }
    }

    #[test]
    fn parameterless_lookalikes_are_not_declarations() {
        // `paramount` is not `param(`, and `end` without a block is a command.
        assert!(!is_script_level_declaration("paramount --version"));
        assert!(!is_script_level_declaration("end-of-line.exe"));
        assert!(!is_script_level_declaration("Get-Process param"));
    }

    #[test]
    fn a_comment_only_command_is_not_a_declaration() {
        assert!(!is_script_level_declaration("# nothing here"));
        assert!(!is_script_level_declaration("<# unterminated"));
    }

    #[test]
    fn epilogue_reads_the_success_flag_before_anything_can_clobber_it() {
        let first_line = EXIT_CODE_EPILOGUE.lines().next().unwrap();
        assert_eq!(first_line, "$__rebon_ok = $?");
    }

    #[test]
    fn spawn_args_pass_the_built_string_after_the_flags() {
        for code_page in [None, GBK] {
            let args = spawn_args("Write-Output hi", code_page);
            assert_eq!(&args[..3], &["-NoProfile", "-NonInteractive", "-Command"]);
            assert_eq!(args[3], build_exec_command("Write-Output hi", code_page));
            assert_eq!(args.len(), 4);
        }
    }

    #[test]
    fn environment_overrides_force_utf8_python_and_plain_output() {
        let overrides = environment_overrides();
        assert!(overrides.contains(&("PYTHONIOENCODING", "utf-8:surrogateescape")));
        // NO_COLOR is conditional on FORCE_COLOR being unset; assert only the
        // invariant that holds either way.
        assert!(overrides.len() <= 2);
    }
}
