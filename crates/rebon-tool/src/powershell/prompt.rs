//! The model-facing PowerShell description.
//!
//! Assembled once per process (the edition cannot change under a running
//! process) and branched on that edition, because pwsh 7 and Windows
//! PowerShell 5.1 do not accept the same syntax. A single merged description
//! would teach the model `&&` on a 5.1 box, where it is a parse error.

use super::detect::PowerShellEdition;

/// Opening paragraph — what the tool is and when *not* to reach for it.
const HEADER: &str = "Executes a given PowerShell command with optional timeout, and returns its \
output.\n\
\n\
This tool is for terminal operations that need a shell: git, package managers, build tools, \
Docker, and PowerShell cmdlets. Do NOT use it for file operations (reading, writing, editing, \
searching, finding files) — use the dedicated tools instead.";

/// For `core`: pwsh 7 has the pipeline chain operators and the
/// native-command error preference, so say so — the model writes better
/// scripts when it can chain instead of squashing everything into `;`.
const EDITION_CORE: &str = "PowerShell edition: PowerShell 7+ (pwsh)\n\
 - Pipeline chain operators `&&` and `||` ARE available and behave like their bash \
counterparts. Prefer `cmd1 && cmd2` over `cmd1; cmd2` when the second command should only run \
if the first succeeded.\n\
 - Ternary (`$cond ? $a : $b`), null-coalescing (`??`), and null-conditional (`?.`) operators \
are available.\n\
 - `$PSNativeCommandUseErrorActionPreference` controls whether a non-zero exit from a native \
executable becomes a terminating error. It is off by default; check `$LASTEXITCODE` yourself \
rather than assuming a failed native command throws.\n\
 - Default file encoding is UTF-8 without BOM.";

/// For `desktop`: 5.1 has none of the above. The absence has to be
/// stated, not merely left out, or the model fills the gap with bash habits.
const EDITION_DESKTOP: &str = "PowerShell edition: Windows PowerShell 5.1\n\
 - `&&` and `||` are NOT available — they are a parse error. Use `;` to sequence statements, \
and test `$?` or `$LASTEXITCODE` between them when the second must only run on success.\n\
 - Ternary, `??`, and `?.` are NOT available; use `if`/`else`.\n\
 - `$PSStyle` does not exist; output is already plain text.\n\
 - Default file encoding is UTF-16LE for `Out-File`/`>` unless you pass `-Encoding utf8`. This \
tool sets `$PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'` for you, but redirection \
operators still need an explicit `Out-File -Encoding utf8`.";

/// The fixed syntax notes that catch out a model whose shell
/// habits are bash-shaped.
const SYNTAX_NOTES: &str = "Syntax notes:\n\
 - The escape character is the backtick (`` ` ``), not the backslash: `` `$ ``, `` `\" ``, \
`` `n ``. A backslash inside a double-quoted string is a literal backslash — Windows paths \
need no escaping.\n\
 - Cmdlets are Verb-Noun: `Get-ChildItem`, `Set-Location`, `New-Item`, `Remove-Item`.\n\
 - Variables take a `$` prefix (`$myVar = \"value\"`); environment variables are read with \
`$env:NAME` and set with `$env:NAME = \"value\"` — there is no inline `VAR=x cmd` prefix form.\n\
 - String interpolation works in double quotes: `\"Hello $name\"`, `\"Hello $($obj.Property)\"`. \
Single quotes are literal.\n\
 - For multi-line literal text use a single-quoted here-string. Its closing `'@` MUST sit at \
column 0 on its own line — indenting it is a parse error:\n\
\n\
    git commit -m @'\n\
    Commit message here.\n\
    Second line with $literal dollar signs.\n\
    '@\n\
\n\
 - `--%` is the stop-parsing token: everything after it is passed to the native program \
verbatim. Use it when PowerShell would otherwise eat an argument, e.g. \
`icacls D:\\data --% /grant Users:(OI)(CI)R`.\n\
 - Call an executable whose path contains spaces through the call operator: \
`& \"C:\\Program Files\\App\\app.exe\" arg1 arg2`.\n\
 - Registry paths use PSDrive prefixes (`HKLM:\\SOFTWARE\\...`), not `HKEY_LOCAL_MACHINE\\...`.";

/// The `-NonInteractive` contract. The tool spawns with
/// `-NonInteractive` and no stdin, so anything that wants a console prompt
/// either fails immediately or burns the whole timeout window waiting.
const NON_INTERACTIVE_WARNING: &str = "Interactive and blocking commands (this tool spawns with \
`-NonInteractive` and stdin connected to the null device, so console prompts read EOF or error \
immediately, while GUI prompts can block until the timeout):\n\
 - NEVER use `Read-Host`, `Get-Credential`, `Out-GridView`, `$Host.UI.PromptForChoice`, or \
`pause`.\n\
 - Destructive cmdlets (`Remove-Item`, `Stop-Process`, `Stop-Service`, `Clear-Content`, …) may \
prompt for confirmation. Pass `-Confirm:$false` when you intend the action to proceed, and \
`-Force` for read-only or hidden items.\n\
 - Never run a command that opens an editor (`git rebase -i`, `git add -i`, `git commit` with \
no `-m`).\n\
 - A command that needs administrator rights returns non-zero rather than elevating. Do not \
retry it — re-run it through `Start-Process -Verb RunAs`, or tell the user what to run.";

/// Steer the model to the dedicated tools. Every `Get-Content` it
/// runs here is output the user has to approve and the transcript has to
/// carry, for something `Read` does better.
const TOOL_SUBSTITUTION: &str = "Avoid using this tool where a dedicated tool exists — they \
produce better output, are cheaper to review, and do not need a shell approval:\n\
 - File search: use Glob (NOT `Get-ChildItem -Recurse`)\n\
 - Content search: use Grep (NOT `Select-String`)\n\
 - Read files: use Read (NOT `Get-Content`)\n\
 - Write files: use Write (NOT `Set-Content`/`Out-File`)\n\
 - Edit files: use Edit (NOT `-replace` round-trips)\n\
 - Communication: output text directly (NOT `Write-Output`/`Write-Host`)";

/// Commands that have no PowerShell equivalent under the same name. Getting
/// these wrong costs a full round-trip on an error the model could have
/// avoided.
const UNIX_COMMAND_MAP: &str = "Unix commands that do NOT exist in PowerShell — use the \
equivalent instead:\n\
 - `head` / `tail` → `Get-Content file -TotalCount N` / `-Tail N`; piped: \
`| Select-Object -First N` / `-Last N`\n\
 - `which` → `(Get-Command name).Source`\n\
 - `wc -l` → `(Get-Content file | Measure-Object -Line).Lines`\n\
 - `mkdir -p` → `New-Item -ItemType Directory -Force path` (`-p` is not a PowerShell flag)\n\
 - `rm -rf` → `Remove-Item -Recurse -Force path`\n\
 - `touch` → `if (-not (Test-Path path)) { New-Item -ItemType File path }` (never \
`New-Item -Force` on an existing file — it truncates it)\n\
 - `ln -s` → `New-Item -ItemType SymbolicLink -Path link -Target target`\n\
 - `chmod` / `chown` → not applicable on Windows; use `icacls` only when ACLs must change\n\
 - `2>/dev/null` → `2>$null`\n\
 - Bash control flow (`if [ -f x ]`, `for x in *`, backtick substitution) is a parse error — \
use `if (Test-Path x)`, `foreach ($x in ...)`, `$(cmd)`";

/// The exceptions, and the operational envelope.
const EXECUTION_NOTES: &str = "Usage notes:\n\
 - The `command` argument is required. `description` is a short active-voice summary shown to \
the user in the approval prompt.\n\
 - `timeout` is in milliseconds (default 60000, max 600000). Set `run_in_background: true` for \
a long-running command instead of raising the timeout: the call returns a `shellId` \
immediately, and ShellOutput / ShellStop read and terminate it.\n\
 - Environment changes do not persist between calls — each call is a fresh `-NoProfile` \
process. Anything that must apply to a later command has to be repeated in that command. The \
one common case worth doing inline is an MSVC environment: import the module and call the \
tool in a single command, e.g. \
`& \"…\\vcvarsall.bat\" x64; cl /? ` inside one call.\n\
 - Quote every path that contains spaces.\n\
 - Do not chain a `cd` prefix onto commands — the working directory is already set for you.\n\
 - For git commits, prefer a new commit over amending, and never skip hooks (`--no-verify`) \
or bypass signing unless the user asked for it.";

/// Assemble the full description for `edition`.
pub fn build_tool_prompt(edition: PowerShellEdition) -> String {
    let edition_notes = match edition {
        PowerShellEdition::Core => EDITION_CORE,
        PowerShellEdition::Desktop => EDITION_DESKTOP,
    };
    format!(
        "{HEADER}\n\n{edition_notes}\n\n{SYNTAX_NOTES}\n\n{NON_INTERACTIVE_WARNING}\n\n\
         {TOOL_SUBSTITUTION}\n\n{UNIX_COMMAND_MAP}\n\n{EXECUTION_NOTES}"
    )
}

/// The short form sent in provider tool metadata when the long description is
/// already carried elsewhere. Still edition-aware — the chaining rule is the
/// one thing a model gets wrong most expensively.
pub fn build_model_description(edition: PowerShellEdition) -> String {
    let chaining = match edition {
        PowerShellEdition::Core => {
            "This is PowerShell 7+: `&&` and `||` are available, and the escape character is \
             the backtick."
        }
        PowerShellEdition::Desktop => {
            "This is Windows PowerShell 5.1: `&&` and `||` are NOT available (use `;`), and the \
             escape character is the backtick."
        }
    };
    format!(
        "Executes a given PowerShell command and returns its output. Use it for terminal \
         operations that need a shell; prefer the dedicated file/search/edit tools otherwise. \
         {chaining} Spawned with `-NoProfile -NonInteractive` and no stdin, so interactive \
         cmdlets (`Read-Host`, `Get-Credential`) will fail — pass `-Confirm:$false` to \
         destructive cmdlets. Supports `timeout` in milliseconds (up to 600000) and \
         `run_in_background`; background calls return a `shellId` for ShellOutput/ShellStop."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_prompt_teaches_chaining_and_desktop_prompt_forbids_it() {
        let core = build_tool_prompt(PowerShellEdition::Core);
        assert!(core.contains("PowerShell 7+"), "{core}");
        assert!(core.contains("`&&` and `||` ARE available"), "{core}");

        let desktop = build_tool_prompt(PowerShellEdition::Desktop);
        assert!(desktop.contains("Windows PowerShell 5.1"), "{desktop}");
        assert!(desktop.contains("are NOT available"), "{desktop}");
        assert!(!desktop.contains("ARE available"), "{desktop}");
    }

    /// The fixed sections are what keep the model out of the
    /// failure modes `-NonInteractive` creates, so both editions carry them.
    #[test]
    fn every_edition_carries_the_fixed_sections() {
        for edition in [PowerShellEdition::Core, PowerShellEdition::Desktop] {
            let prompt = build_tool_prompt(edition);
            for required in [
                "backtick",
                "Verb-Noun",
                "$env:NAME",
                "'@",
                "--%",
                "Read-Host",
                "-Confirm:$false",
                "use Glob",
                "use Grep",
                "use Read",
                "use Write",
                "run_in_background",
            ] {
                assert!(
                    prompt.contains(required),
                    "{edition:?} prompt missing {required:?}"
                );
            }
        }
    }

    #[test]
    fn model_description_states_the_chaining_rule_for_the_edition() {
        assert!(build_model_description(PowerShellEdition::Core).contains("are available"));
        assert!(build_model_description(PowerShellEdition::Desktop)
            .contains("are NOT available (use `;`)"));
    }
}
