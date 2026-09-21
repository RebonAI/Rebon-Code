//! Forwarding shells for the entrypoints that became binaries of their own.
//!
//! `browser-mcp`, `lsp-mcp` and `computer-use` used to be implemented inside
//! `rebon`. They are now `rebon-browser-mcp`, `rebon-lsp-mcp` and
//! `rebon-computer-use`, shipped beside it and owned by the plugins whose
//! features they are. The old
//! subcommands survive one release as shells that hand their arguments over
//! verbatim, so a config file or a habit written against 0.24 keeps working
//! through 0.25. They go away in 0.26.
//!
//! Two rules make the shells safe to sit in front of an MCP server:
//!
//! - **Nothing is written to stdout.** For `browser-mcp` and `lsp-mcp` stdout
//!   is the protocol channel; a single stray byte breaks the client that
//!   spawned this process. The deprecation notice and every error go to
//!   stderr.
//! - **The shell does not link the feature.** It resolves a path and hands the
//!   process over, which is what lets `rebon-cli` drop its dependencies on the
//!   browser and LSP crates in the same release.

use std::ffi::OsString;
use std::process::Command as ProcessCommand;

use rebon_types::sibling_binary::{self, SiblingBinaryMissing};

/// Exit code for "the binary this forwards to is not installed".
///
/// 127 is the shell convention for "command not found", which is exactly the
/// situation: `rebon` is here, the executable it would hand over to is not.
const SIBLING_NOT_FOUND_EXIT_CODE: i32 = 127;

/// One line naming the replacement, for stderr.
fn deprecation_notice(old: &str, sibling: &str) -> String {
    format!(
        "warning: `rebon {old}` is deprecated and will be removed in 0.26; \
         run `{sibling}` instead. Forwarding."
    )
}

/// What to print and what to exit with when the sibling is missing.
///
/// Split out so the failure is testable without spawning a process: the
/// caller writes the text to stderr and nothing to stdout.
fn missing_sibling_report(old: &str, error: &SiblingBinaryMissing) -> (String, i32) {
    (format!("rebon {old}: {error}"), SIBLING_NOT_FOUND_EXIT_CODE)
}

/// Hands this process over to `sibling`, passing `args` through unchanged.
///
/// Never returns: on Unix the image is replaced, so signals, the process
/// group and every inherited descriptor keep the semantics the caller's
/// client already relies on; on Windows the child is spawned into the same
/// console (so Ctrl+C reaches both) and this process exits with the child's
/// code.
///
/// This exits the process directly rather than returning a code up through
/// `route_main`. None of the three routes starts the plugin plane, so there is
/// nothing for `async_main`'s shutdown to do on the way out.
pub(crate) fn forward_to_sibling(old: &str, sibling: &str, args: Vec<OsString>) -> ! {
    let path = match sibling_binary::resolve(sibling) {
        Ok(path) => path,
        Err(error) => {
            let (message, code) = missing_sibling_report(old, &error);
            eprintln!("{message}");
            std::process::exit(code);
        }
    };
    eprintln!("{}", deprecation_notice(old, sibling));

    let mut command = ProcessCommand::new(&path);
    command.args(&args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns if the exec itself failed.
        let error = command.exec();
        eprintln!("rebon {old}: failed to run {}: {error}", path.display());
        std::process::exit(SIBLING_NOT_FOUND_EXIT_CODE);
    }

    #[cfg(not(unix))]
    {
        match command.spawn().and_then(|mut child| child.wait()) {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => {
                eprintln!("rebon {old}: failed to run {}: {error}", path.display());
                std::process::exit(SIBLING_NOT_FOUND_EXIT_CODE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_missing_sibling_is_command_not_found() {
        let directory = tempfile::tempdir().expect("temp dir");
        let executable = directory.path().join("rebon");
        let error = sibling_binary::resolve_from_executable("rebon-lsp-mcp", &executable)
            .expect_err("nothing installed beside the executable");
        let (message, code) = missing_sibling_report("lsp-mcp", &error);
        assert_eq!(code, 127);
        assert!(message.contains("rebon-lsp-mcp"), "{message}");
        assert!(message.starts_with("rebon lsp-mcp:"), "{message}");
    }

    #[test]
    fn the_notice_names_the_binary_that_replaces_the_subcommand() {
        let notice = deprecation_notice("browser-mcp", "rebon-browser-mcp");
        assert!(notice.contains("rebon browser-mcp"), "{notice}");
        assert!(notice.contains("rebon-browser-mcp"), "{notice}");
        assert!(notice.contains("0.26"), "{notice}");
    }

    #[test]
    fn every_forwarded_name_is_a_sibling_candidate_and_never_a_bare_name() {
        // A bare file name is what `CreateProcess`/`execvp` would resolve off
        // PATH; the locator refuses to produce one, and these three names are
        // what the shells hand it.
        for sibling in [
            crate::BROWSER_MCP_SIBLING,
            crate::LSP_MCP_SIBLING,
            crate::COMPUTER_USE_SIBLING,
        ] {
            assert!(sibling.starts_with("rebon-"), "{sibling}");
            let candidates = sibling_binary::candidates(
                sibling,
                &Path::new("install").join(sibling_binary::file_name("rebon")),
            );
            assert_eq!(
                candidates[0],
                Path::new("install").join(sibling_binary::file_name(sibling))
            );
        }
    }
}
