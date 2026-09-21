//! The real `CommandLineToArgvW`.
//!
//! [`crate::core::cmdline`] re-quotes the confined process's argv into a single
//! command line, and carries a reference implementation of the parser so the
//! round trip can be tested on any host. This module exposes the function
//! Windows will *actually* use, so the two can be checked against each other on a
//! Windows runner.
//!
//! That check is worth a module of its own. The re-quoting is the one place in
//! the helper where a subtle error is invisible: a mis-escaped trailing
//! backslash does not fail, it silently merges two arguments into one, and the
//! confined process runs a command nobody wrote.
//!
//! `CommandLineToArgvW` reads `argv[0]` by different rules than the rest, so the
//! tests keep a plain program name in front rather than pretending the quirk does
//! not exist.

use crate::sys::SysResult;

/// Split a command line the way Windows will.
pub fn parse(command_line: &str) -> SysResult<Vec<String>> {
    imp::parse(command_line)
}

#[cfg(windows)]
mod imp {
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    pub fn parse(command_line: &str) -> SysResult<Vec<String>> {
        // `CommandLineToArgvW` treats an empty string as "the current executable", which
        // is a surprise the helper never wants; an empty command line is simply no
        // arguments.
        if command_line.is_empty() {
            return Ok(Vec::new());
        }

        let wide: Vec<u16> = std::ffi::OsStr::new(command_line)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut count = 0i32;
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
        if argv.is_null() {
            return Err(SysError::win32("CommandLineToArgvW", unsafe {
                GetLastError()
            }));
        }

        let mut parsed = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count.max(0) as usize {
            let pointer = unsafe { *argv.add(index) };
            let mut length = 0usize;
            while unsafe { *pointer.add(length) } != 0 {
                length += 1;
            }
            let slice = unsafe { std::slice::from_raw_parts(pointer, length) };
            parsed.push(String::from_utf16_lossy(slice));
        }
        unsafe { LocalFree(argv as *mut c_void) };
        Ok(parsed)
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::sys::{SysError, SysResult};

    pub fn parse(_command_line: &str) -> SysResult<Vec<String>> {
        Err(SysError::Unsupported("CommandLineToArgvW"))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::core::cmdline::{build_command_line, parse_command_line};

    /// Round trip an argv through the core's quoting and the OS's parser.
    fn through_windows(arguments: &[&str]) -> Vec<String> {
        let mut argv = vec!["prog.exe".to_string()];
        argv.extend(arguments.iter().map(|a| (*a).to_string()));
        parse(&build_command_line(&argv))
            .unwrap()
            .into_iter()
            .skip(1)
            .collect()
    }

    fn assert_survives(arguments: &[&str]) {
        let expected: Vec<String> = arguments.iter().map(|a| (*a).to_string()).collect();
        assert_eq!(through_windows(arguments), expected, "argv {arguments:?}");
    }

    #[test]
    fn a_plain_argv_survives() {
        assert_survives(&["exec", "--quiet", "--"]);
    }

    #[test]
    fn the_encoded_command_shape_survives_the_real_parser() {
        // The reason argv is passed through untouched, checked against the function that
        // will actually read it back.
        assert_survives(&[
            "pwsh.exe",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            "ZQBjAGgAbwAgACIAaABlAGwAbABvACIA",
        ]);
    }

    #[test]
    fn a_trailing_backslash_does_not_swallow_the_next_argument() {
        // The failure mode this module exists for: get the escaping wrong and `C:\dir\`
        // runs into the argument after it, and the confined process receives a command
        // line nobody wrote.
        assert_survives(&[r"C:\dir with space\", "--next", "value"]);
    }

    #[test]
    fn quotes_and_backslashes_survive_in_every_arrangement() {
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
            r#"{"key": "value \\ done"}"#,
            "中文",
            "\u{1F600}",
        ];
        for first in pieces {
            for second in pieces {
                assert_survives(&[first, second]);
            }
        }
    }

    #[test]
    fn the_cores_reference_parser_agrees_with_windows() {
        // If these ever diverge, every test in `crate::core::cmdline` is checking the
        // wrong function, and it would keep passing.
        let pieces = [
            "",
            "a b",
            r"C:\dir\",
            r#"say "hi""#,
            r#"a\"b"#,
            r"\\server\share",
            "a\\\\\"b",
        ];
        for first in pieces {
            for second in pieces {
                let mut argv = vec!["prog.exe".to_string()];
                argv.push((*first).to_string());
                argv.push((*second).to_string());
                let line = build_command_line(&argv);
                assert_eq!(
                    parse(&line).unwrap(),
                    parse_command_line(&line),
                    "command line {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_quoted_program_path_with_spaces_is_one_argument() {
        let argv = vec![
            r"C:\Program Files\Rebon\sandbox-win.exe".to_string(),
            "status".into(),
        ];
        assert_eq!(parse(&build_command_line(&argv)).unwrap(), argv);
    }

    #[test]
    fn an_empty_command_line_is_no_arguments_not_this_executable() {
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn an_empty_argument_keeps_its_position() {
        // Losing it would shift every later argument one place left, so
        // `--deny-write "" --allow-write C:\work` would deny the write root.
        assert_survives(&["--deny-write", "", "--allow-write"]);
    }
}
