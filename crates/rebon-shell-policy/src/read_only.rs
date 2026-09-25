//! Shell commands that only read, and so need nobody's approval.
//!
//! Plan mode asks the model to explore before it changes anything, and the
//! exploring is `ls`, `git log`, `rg`. When every one of those stopped for a
//! prompt, the reading the mode exists for was the thing it made expensive.
//! A command this module accepts runs without a prompt in every mode; the
//! `Read` tool already reads any path without one, so nothing new is trusted.
//!
//! The judgment is an allowlist and it fails closed. A command is read-only
//! only when it is a list of plain argvs — [`crate::lexer::READ_ONLY_ARGV`]
//! refuses redirection, substitution, expansion and globbing before anything
//! here looks at it — and every link names a program on the list below with
//! arguments that program cannot be talked into writing, deleting, running
//! something else or reaching the network with. Anything unlisted, and
//! anything this cannot read, asks as before.

use serde_json::Value;

use crate::lexer::{simple_command_segments, READ_ONLY_ARGV};
use crate::powershell_shape::{canonical_cmdlet, powershell_segments};

/// Whether `input`'s command only reads.
///
/// Never true for a command that names a live session's credentials: the
/// file tools refuse those paths outright, and a shell spelling of the same
/// read keeps its prompt (see [`crate::is_session_credential_access_command`]).
pub fn is_read_only_shell_command(tool_name: &str, input: &Value) -> bool {
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return false;
    };
    if crate::is_session_credential_access_command(tool_name, input) {
        return false;
    }
    match tool_name {
        "Bash" | "BashTool" => simple_command_segments(command.trim(), &READ_ONLY_ARGV)
            .is_ok_and(|segments| segments.iter().all(|argv| argv_only_reads(argv))),
        "PowerShell" | "PowerShellTool" => powershell_segments(command, true)
            .is_some_and(|segments| segments.iter().all(|argv| powershell_argv_only_reads(argv))),
        _ => false,
    }
}

/// One POSIX argv: a program that only reads, with arguments that keep it so.
fn argv_only_reads(argv: &[String]) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    // `/proc/<pid>/environ` is every environment variable of a process,
    // secrets included — the POSIX spelling of PowerShell's `Env:` drive.
    // Anything under `/proc` asks, so `grep -r` cannot walk into it either.
    if args
        .iter()
        .any(|arg| arg == "/proc" || arg.starts_with("/proc/"))
    {
        return false;
    }
    match program.as_str() {
        // Print, list or measure what they are given, and have no option
        // that writes a file or runs another program.
        "cat" | "head" | "tail" | "wc" | "ls" | "pwd" | "echo" | "grep" | "egrep" | "fgrep"
        | "stat" | "du" | "df" | "which" | "whoami" | "uname" | "basename" | "dirname"
        | "realpath" | "readlink" | "diff" | "cmp" | "cut" | "tr" | "nl" | "true" | "false" => true,
        "find" => find_args_only_read(args),
        "rg" => rg_args_only_read(args),
        "sort" => sort_args_only_read(args),
        // `uniq IN OUT` writes OUT; with no file operand it is a filter.
        "uniq" => args.iter().all(|arg| arg.starts_with('-')),
        "git" => git_args_only_read(args),
        "cargo" => cargo_args_only_read(args),
        _ => false,
    }
}

/// `find` runs, deletes and writes through its own expressions.
fn find_args_only_read(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-exec"
                | "-execdir"
                | "-ok"
                | "-okdir"
                | "-delete"
                | "-fprint"
                | "-fprint0"
                | "-fprintf"
                | "-fls"
        )
    })
}

/// `rg --pre` and `--hostname-bin` each run a command of the caller's choosing.
fn rg_args_only_read(args: &[String]) -> bool {
    !args
        .iter()
        .any(|arg| arg.starts_with("--pre") || arg.starts_with("--hostname-bin"))
}

/// `sort -o` writes its output to a file, and `--compress-program` runs one.
fn sort_args_only_read(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        arg.starts_with("--output")
            || arg.starts_with("--compress-program")
            || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('o'))
    })
}

/// Git subcommands that inspect the repository, with the few options that
/// would turn one of them into a writer or a launcher refused.
///
/// Global options other than `-C <dir>` and `--no-pager` are refused: `-c`
/// sets any config for the one run, pager and diff drivers included.
fn git_args_only_read(args: &[String]) -> bool {
    let mut rest = args;
    loop {
        match rest {
            [flag, _dir, tail @ ..] if flag == "-C" => rest = tail,
            [flag, tail @ ..] if flag == "--no-pager" || flag == "-P" => rest = tail,
            _ => break,
        }
    }
    let Some((subcommand, sub_args)) = rest.split_first() else {
        return false;
    };
    // `--output` writes the patch to a file, `--ext-diff` runs a diff driver,
    // and `grep -O` opens the matches in a pager program.
    if sub_args.iter().any(|arg| {
        arg.starts_with("--output")
            || arg.starts_with("--ext-diff")
            || arg.starts_with("--open-files-in-pager")
            || (subcommand == "grep" && arg.starts_with("-O"))
    }) {
        return false;
    }
    let first = sub_args.first().map(String::as_str);
    match subcommand.as_str() {
        "status" | "log" | "show" | "diff" | "blame" | "shortlog" | "describe" | "rev-parse"
        | "rev-list" | "ls-files" | "ls-tree" | "cat-file" | "merge-base" | "show-ref"
        | "name-rev" | "grep" | "whatchanged" | "count-objects" | "version" => true,
        // Listing only: a name creates, and the flags below move or delete.
        "branch" => sub_args.iter().all(|arg| {
            matches!(
                arg.as_str(),
                "-a" | "-r" | "-v" | "-vv" | "--all" | "--remotes" | "--show-current" | "--list"
            )
        }),
        "tag" => first.is_none() || matches!(first, Some("-l" | "--list")),
        // `remote show` and `remote update` reach the network.
        "remote" => sub_args.iter().all(|arg| arg == "-v" || arg == "--verbose"),
        "stash" | "worktree" => {
            matches!(first, Some("list")) || (subcommand == "stash" && first == Some("show"))
        }
        "reflog" => !matches!(first, Some("expire" | "delete")),
        "config" => {
            sub_args.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "--get" | "--get-all" | "--get-regexp" | "--list" | "-l"
                )
            }) && !sub_args.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "--add"
                        | "--unset"
                        | "--unset-all"
                        | "--replace-all"
                        | "--rename-section"
                        | "--remove-section"
                        | "-e"
                        | "--edit"
                )
            })
        }
        _ => false,
    }
}

/// `cargo metadata --no-deps` reads the workspace's own manifests; without
/// `--no-deps` it resolves the graph, which can write `Cargo.lock` and reach
/// the registry.
fn cargo_args_only_read(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => args.len() == 1,
        Some("metadata") => args.iter().any(|arg| arg == "--no-deps"),
        _ => false,
    }
}

/// Cmdlets that read the filesystem or shape what is already in the
/// pipeline. None of them takes a script block — the shape parser refuses
/// `{` anyway — and none has a parameter that writes.
const READ_ONLY_CMDLETS: &[&str] = &[
    "Get-ChildItem",
    "Get-Content",
    "Select-String",
    "Get-Location",
    "Get-Item",
    "Get-ItemProperty",
    "Test-Path",
    "Resolve-Path",
    "Split-Path",
    "Join-Path",
    "Measure-Object",
    "Select-Object",
    "Sort-Object",
    "Format-Table",
    "Format-List",
    "Out-String",
    "Write-Output",
    "Get-Command",
    "Get-FileHash",
];

/// One PowerShell argv: a read-only cmdlet (by name or built-in alias), or a
/// native program that passes the POSIX judgment with nothing PowerShell
/// would expand in its arguments. A cmdlet binds `$x` as one value; a native
/// program gets whatever `$x` or `@x` expands to, which can be an option.
fn powershell_argv_only_reads(argv: &[String]) -> bool {
    let Some(name) = argv.first() else {
        return false;
    };
    if argv
        .iter()
        .any(|word| names_a_non_filesystem_provider(word))
    {
        return false;
    }
    let cmdlet = canonical_cmdlet(name);
    if READ_ONLY_CMDLETS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(cmdlet))
    {
        return true;
    }
    if !cmdlet.eq_ignore_ascii_case(name) {
        // An alias for some other cmdlet (`rm`, `sc`, `iex`).
        return false;
    }
    let expands = argv
        .iter()
        .any(|word| word.contains('$') || word.starts_with('@'));
    !expands && native_program_only_reads(argv)
}

/// PowerShell drives that are not a filesystem. The item cmdlets read them
/// like directories: `Get-ChildItem Env:` lists every environment variable,
/// API keys included, and `HKLM:` is the registry. Reading one is not the
/// read the allowlist vouches for.
const NON_FILESYSTEM_DRIVES: &[&str] = &[
    "env", "variable", "function", "alias", "cert", "hklm", "hkcu", "wsman",
];

/// Whether `word` names a non-filesystem provider: one of the drives above,
/// with or without a path after it and in any case — bare (`Env:`), as a
/// parameter value (`-Path:Env:X`), in a list (`a,Env:X`) or as a variable
/// (`$env:KEY`, which is the same drive by another spelling) — or a
/// provider-qualified path (`Registry::HKEY_…`,
/// `Microsoft.PowerShell.Core\Registry::…`). A drive letter (`C:`) is one
/// character and matches none of them.
fn names_a_non_filesystem_provider(word: &str) -> bool {
    if word.contains("::") {
        return true;
    }
    let word = word.to_ascii_lowercase();
    NON_FILESYSTEM_DRIVES.iter().any(|drive| {
        let needle = format!("{drive}:");
        word.match_indices(&needle).any(|(at, _)| {
            word[..at]
                .chars()
                .next_back()
                .is_none_or(|before| !(before.is_ascii_alphanumeric() || before == '_'))
        })
    })
}

/// The POSIX judgment for a native program started from PowerShell, where
/// the few names PowerShell does not alias still mean the same executable.
fn native_program_only_reads(argv: &[String]) -> bool {
    matches!(argv[0].as_str(), "git" | "rg" | "cargo") && argv_only_reads(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> bool {
        is_read_only_shell_command("Bash", &json!({ "command": command }))
    }

    fn powershell(command: &str) -> bool {
        is_read_only_shell_command("PowerShell", &json!({ "command": command }))
    }

    #[test]
    fn plain_reads_run_in_bash() {
        for command in [
            "ls",
            "ls -la src",
            "cat Cargo.toml",
            "head -n 20 README.md",
            "tail -f log.txt",
            "wc -l src/lib.rs",
            "pwd",
            "echo hello",
            "grep -rn TODO crates",
            "rg --files",
            "rg -n 'fn main' crates",
            "find . -name Cargo.toml -type f",
            "git status",
            "git status --short",
            "git log --oneline -5",
            "git diff HEAD -- src",
            "git show HEAD:README.md",
            "git -C crates/rebon-core log -1",
            "git --no-pager log -3",
            "git branch -a",
            "git branch --show-current",
            "git tag -l",
            "git remote -v",
            "git stash list",
            "git worktree list",
            "git config --get user.name",
            "git rev-parse --show-toplevel",
            "cargo metadata --no-deps --format-version 1",
            "cargo --version",
            "git log --oneline | head -20",
            "rg -c foo | sort | uniq -c",
            "ls && git status; pwd",
            "stat Cargo.toml || true",
        ] {
            assert!(bash(command), "expected {command:?} to be read-only");
        }
    }

    #[test]
    fn anything_that_writes_runs_or_expands_still_asks_in_bash() {
        for command in [
            // Not on the list at all.
            "rm -rf target",
            "touch x",
            "mkdir out",
            "sed -i s/a/b/ f",
            "awk '{print}' f",
            "xargs cat",
            "env ls",
            // The environment, secrets included, by any spelling.
            "env",
            "printenv",
            "printenv OPENAI_API_KEY",
            "cat /proc/self/environ",
            "grep -r KEY /proc",
            "sudo ls",
            "curl https://example.com",
            "python -c 'print(1)'",
            // A variable assignment in front changes what runs.
            "GIT_PAGER=evil git log",
            "LD_PRELOAD=x.so ls",
            // Redirection, substitution, expansion, globbing, backgrounding.
            "ls > files.txt",
            "cat a >> b",
            "echo x 2>/dev/null",
            "cat < in",
            "echo $(rm -rf /)",
            "echo `id`",
            "cat $HOME/.bashrc",
            "ls *.rs",
            "ls ~",
            "ls &",
            "ls\nrm x",
            // A read piped or chained into something that is not one.
            "git log | sh",
            "ls && rm x",
            "cat f; touch g",
            "ls || rm x",
            "ls |& cat",
            // Read-only programs talked into writing or launching.
            "find . -delete",
            "find . -name x -exec rm {} ;",
            "find . -execdir sh -c x ;",
            "find . -fprint out",
            "rg --pre ./script foo",
            "rg --pre=./script foo",
            "rg --hostname-bin=./x foo",
            "sort -o out in",
            "sort -uo out in",
            "sort --output=out in",
            "sort --compress-program=sh in",
            "uniq in out",
            // Git that writes, runs a driver or reaches the network.
            "git commit -m x",
            "git checkout main",
            "git push",
            "git fetch",
            "git pull",
            "git -c core.pager=sh log",
            "git diff --output=patch",
            "git log --output=x",
            "git diff --ext-diff",
            "git grep -O foo",
            "git grep --open-files-in-pager=vim foo",
            "git branch new-branch",
            "git branch -D old",
            "git tag v1",
            "git remote show origin",
            "git remote add x y",
            "git stash",
            "git stash drop",
            "git worktree remove x",
            "git reflog expire --all",
            "git config user.name x",
            "git config --unset --get x",
            "git",
            // Cargo that resolves, builds or runs.
            "cargo metadata",
            "cargo build",
            "cargo run",
            "cargo --version --list",
            // Unreadable.
            "echo 'unterminated",
            "",
        ] {
            assert!(!bash(command), "expected {command:?} to ask");
        }
    }

    #[test]
    fn a_session_credential_read_still_asks() {
        assert!(!bash("cat /home/u/.rebon/jobs/abc/state.json"));
        assert!(!bash("cat sess.owner.json"));
        assert!(!powershell(
            "Get-Content C:\\Users\\u\\.rebon\\jobs\\a\\state.json"
        ));
    }

    #[test]
    fn a_tool_that_is_not_a_shell_is_never_read_only() {
        assert!(!is_read_only_shell_command(
            "Monitor",
            &json!({ "command": "ls" })
        ));
        assert!(!is_read_only_shell_command("Bash", &json!({})));
    }

    #[test]
    fn plain_reads_run_in_powershell() {
        for command in [
            "Get-ChildItem",
            "Get-ChildItem -Recurse -Filter *.rs src",
            "gci src",
            "ls",
            "dir C:\\repo",
            "Get-Content Cargo.toml",
            "gc README.md -TotalCount 20",
            "cat Cargo.toml",
            "Select-String -Path src\\*.rs -Pattern TODO",
            "sls TODO src\\lib.rs",
            "Get-Location",
            "pwd",
            "Test-Path Cargo.toml",
            "Get-Content $HOME\\notes.txt",
            "Get-ChildItem | Select-Object -First 5",
            "Get-Content log.txt | Select-String error | Measure-Object",
            "Get-ChildItem -Recurse | Sort-Object Length | Format-Table",
            "git status",
            "git log --oneline -5 | Select-Object -First 3",
            "rg -n TODO crates",
            "Get-Location; git status",
        ] {
            assert!(powershell(command), "expected {command:?} to be read-only");
        }
    }

    #[test]
    fn anything_that_writes_runs_or_expands_still_asks_in_powershell() {
        for command in [
            "Remove-Item x",
            "rm x",
            "del x",
            "Set-Content x y",
            "sc x y",
            "Out-File x",
            "Get-ChildItem | Remove-Item",
            "Get-Content x | Set-Content y",
            "Get-Content x > y",
            "Get-ChildItem | Where-Object { $_.Length -gt 0 }",
            "Get-ChildItem | ForEach-Object { Remove-Item $_ }",
            "iex 'Get-ChildItem'",
            "Invoke-Expression x",
            "Start-Process notepad",
            "& git status",
            "Get-Content (Get-Item x)",
            "Get-Content \"$(Remove-Item x)\"",
            "Get-ChildItem --% x",
            "git commit -m x",
            "git $args",
            "git @flags",
            "git log --output=x",
            "curl.exe https://example.com",
            "Invoke-WebRequest https://example.com",
            "Get-ChildItem |",
            "",
        ] {
            assert!(!powershell(command), "expected {command:?} to ask");
        }
    }

    /// The item cmdlets read every provider, not only the filesystem:
    /// `Get-ChildItem Env:` is the environment, API keys included, and
    /// `HKLM:` the registry. A non-filesystem drive or a provider-qualified
    /// path asks, however it is spelled.
    #[test]
    fn a_non_filesystem_provider_asks_in_powershell() {
        for command in [
            "Get-ChildItem Env:",
            "gci env:",
            "ls ENV:",
            "Get-Content Env:OPENAI_API_KEY",
            "Get-Item -Path Env:PATH",
            "Get-Item -Path:Env:PATH",
            "Get-Content a.txt,Env:SECRET",
            "Write-Output $env:OPENAI_API_KEY",
            "Get-ChildItem Variable:",
            "Get-ChildItem Function:\\prompt",
            "Get-ChildItem Alias:",
            "Get-ChildItem Cert:\\CurrentUser\\My",
            "Get-ItemProperty HKLM:\\SOFTWARE\\Microsoft",
            "Get-ChildItem hkcu:\\Software",
            "Test-Path WSMan:\\localhost",
            "Resolve-Path Registry::HKEY_LOCAL_MACHINE\\SOFTWARE",
            "Get-ChildItem Microsoft.PowerShell.Core\\Registry::HKEY_CURRENT_USER",
            "Get-Location; gci Env:",
            "git log -1 | Select-Object Env:X",
        ] {
            assert!(!powershell(command), "expected {command:?} to ask");
        }
    }

    /// A drive letter is a filesystem drive, and a name that merely contains
    /// a provider's name is not one.
    #[test]
    fn a_filesystem_drive_still_runs_in_powershell() {
        for command in [
            "Get-ChildItem C:",
            "Get-ChildItem C:\\repo\\src",
            "Get-Content d:\\notes\\environment.md",
            "Test-Path C:\\Windows\\System32",
            "Get-ChildItem .\\myenv",
            "Get-Item C:\\repo\\myenv:stream",
        ] {
            assert!(powershell(command), "expected {command:?} to be read-only");
        }
    }
}
