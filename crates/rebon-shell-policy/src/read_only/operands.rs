//! Which words of a read-only argv name a file or directory it reads.
//!
//! The rules that guard the Read tool are path rules, and a read made
//! through the shell has to answer to them too. This picks the path
//! operands out of the argvs [`super::argv_only_reads`] accepts, by each
//! program's own grammar: a grep pattern, a sed script, an option's value
//! and stdin's `-` are not paths. Where the grammar is not modelled the
//! words are kept, so an unknown option's value can only be judged as a
//! path too, never let a path through.
//!
//! A command with no path operand (`rg foo`, `ls`) reads under the working
//! directory; it names no path, and is left to the ordinary decision.

/// How one program takes its operands.
struct Grammar {
    /// Short options that take a value, inline (`-n20`) or as the next word.
    short_values: &'static str,
    /// Long options that take a value, inline (`--lines=20`) or as the next word.
    long_values: &'static [&'static str],
    /// What the first operand is when nothing else supplied it.
    first: First,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum First {
    /// Every operand is a path.
    Path,
    /// The first operand is a pattern or script unless an option gave one;
    /// these short and long options give one.
    Pattern {
        short: char,
        long: &'static str,
        /// Options whose value is a file the pattern is read from.
        file_short: Option<char>,
        file_long: Option<&'static str>,
    },
}

const PATHS: Grammar = Grammar {
    short_values: "",
    long_values: &[],
    first: First::Path,
};

const GREP: Grammar = Grammar {
    short_values: "efABCmDd",
    long_values: &[
        "regexp",
        "file",
        "after-context",
        "before-context",
        "context",
        "max-count",
        "include",
        "exclude",
        "exclude-dir",
        "exclude-from",
        "label",
        "color",
        "colour",
        "binary-files",
        "devices",
        "directories",
    ],
    first: First::Pattern {
        short: 'e',
        long: "regexp",
        file_short: Some('f'),
        file_long: Some("file"),
    },
};

const RG: Grammar = Grammar {
    short_values: "efABCmgtTMjErd",
    long_values: &[
        "regexp",
        "file",
        "after-context",
        "before-context",
        "context",
        "max-count",
        "glob",
        "iglob",
        "type",
        "type-not",
        "type-add",
        "type-clear",
        "max-columns",
        "threads",
        "encoding",
        "replace",
        "max-depth",
        "max-filesize",
        "path-separator",
        "colors",
        "color",
        "sort",
        "sortr",
        "context-separator",
        "field-match-separator",
        "field-context-separator",
        "engine",
        "dfa-size-limit",
        "regex-size-limit",
        "ignore-file",
    ],
    first: First::Pattern {
        short: 'e',
        long: "regexp",
        file_short: Some('f'),
        file_long: Some("file"),
    },
};

const SED: Grammar = Grammar {
    short_values: "el",
    long_values: &["expression", "line-length"],
    first: First::Pattern {
        short: 'e',
        long: "expression",
        file_short: None,
        file_long: None,
    },
};

/// `fd PATTERN [PATH]...`; the pattern is never given by an option.
const FD: Grammar = Grammar {
    short_values: "edtEcjSo",
    long_values: &[
        "extension",
        "max-depth",
        "min-depth",
        "exact-depth",
        "type",
        "exclude",
        "color",
        "threads",
        "size",
        "owner",
        "changed-within",
        "changed-before",
        "max-results",
        "path-separator",
        "batch-size",
        "ignore-file",
        "format",
        "search-path",
        "base-directory",
    ],
    first: First::Pattern {
        short: '\0',
        long: "",
        file_short: None,
        file_long: None,
    },
};

fn grammar(program: &str) -> Option<Grammar> {
    Some(match program {
        // Print or measure what they are given.
        "cat" | "wc" | "diff" | "cmp" => PATHS,
        "head" | "tail" => Grammar {
            short_values: "nc",
            long_values: &["lines", "bytes", "pid", "sleep-interval"],
            ..PATHS
        },
        "ls" => Grammar {
            short_values: "IwT",
            long_values: &[
                "ignore",
                "hide",
                "width",
                "tabsize",
                "format",
                "sort",
                "time",
                "time-style",
                "color",
                "indicator-style",
                "quoting-style",
                "block-size",
            ],
            ..PATHS
        },
        "stat" => Grammar {
            short_values: "fc",
            long_values: &["format", "printf"],
            ..PATHS
        },
        "du" => Grammar {
            short_values: "dBtX",
            long_values: &[
                "max-depth",
                "block-size",
                "threshold",
                "exclude",
                "time-style",
            ],
            ..PATHS
        },
        "cut" => Grammar {
            short_values: "bcdf",
            long_values: &[
                "bytes",
                "characters",
                "delimiter",
                "fields",
                "output-delimiter",
            ],
            ..PATHS
        },
        "nl" => Grammar {
            short_values: "bhfvisdwn",
            long_values: &[
                "body-numbering",
                "header-numbering",
                "footer-numbering",
                "starting-line-number",
                "line-increment",
                "join-blank-lines",
                "number-separator",
                "section-delimiter",
                "number-width",
                "number-format",
            ],
            ..PATHS
        },
        "sort" => Grammar {
            short_values: "kStT",
            long_values: &[
                "key",
                "buffer-size",
                "field-separator",
                "temporary-directory",
                "parallel",
                "sort",
                "random-source",
                "files0-from",
            ],
            ..PATHS
        },
        "uniq" => Grammar {
            short_values: "fsw",
            long_values: &[
                "skip-fields",
                "skip-chars",
                "check-chars",
                "group",
                "all-repeated",
            ],
            ..PATHS
        },
        "grep" | "egrep" | "fgrep" => GREP,
        "rg" => RG,
        "sed" => SED,
        "fd" | "fdfind" => FD,
        _ => return None,
    })
}

/// The words of `argv` that name a file or directory it reads.
pub(super) fn read_paths(argv: &[String]) -> Vec<String> {
    let Some((program, args)) = argv.split_first() else {
        return Vec::new();
    };
    match program.as_str() {
        "find" => find_roots(args),
        "git" => git_paths(args),
        "rg" if args.iter().any(|arg| arg == "--files") => operands(args, &RG).paths,
        program => match grammar(program) {
            Some(grammar) => {
                let found = operands(args, &grammar);
                let mut paths = found.paths;
                if matches!(grammar.first, First::Pattern { .. })
                    && !found.pattern_given
                    && !paths.is_empty()
                {
                    paths.remove(0);
                }
                paths.extend(found.value_paths);
                paths
            }
            // Programs that read no file's content: `echo`, `pwd`, `which`,
            // `tr` on its stdin, `basename`, `cargo metadata`.
            None => Vec::new(),
        },
    }
}

struct Operands {
    /// The operands, in order, the first possibly a pattern.
    paths: Vec<String>,
    /// Whether an option supplied the pattern or script.
    pattern_given: bool,
    /// Paths given as option values (a pattern file, fd's search path).
    value_paths: Vec<String>,
}

fn operands(args: &[String], grammar: &Grammar) -> Operands {
    let mut found = Operands {
        paths: Vec::new(),
        pattern_given: false,
        value_paths: Vec::new(),
    };
    let (pattern_short, pattern_long, file_short, file_long) = match grammar.first {
        First::Pattern {
            short,
            long,
            file_short,
            file_long,
        } => (Some(short), Some(long), file_short, file_long),
        First::Path => (None, None, None, None),
    };
    let mut words = args.iter();
    while let Some(arg) = words.next() {
        if arg == "--" {
            found.paths.extend(words.by_ref().cloned());
            break;
        }
        if arg == "-" {
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            if !grammar.long_values.contains(&name) {
                continue;
            }
            let value = inline.or_else(|| words.next().cloned());
            if Some(name) == pattern_long {
                found.pattern_given = true;
            } else if Some(name) == file_long || matches!(name, "search-path" | "base-directory") {
                found.pattern_given |= Some(name) == file_long;
                found.value_paths.extend(value);
            }
            continue;
        }
        if let Some(cluster) = arg.strip_prefix('-').filter(|cluster| !cluster.is_empty()) {
            for (at, flag) in cluster.char_indices() {
                if !grammar.short_values.contains(flag) {
                    continue;
                }
                let rest = &cluster[at + flag.len_utf8()..];
                let value = if rest.is_empty() {
                    words.next().cloned()
                } else {
                    Some(rest.to_string())
                };
                if Some(flag) == pattern_short {
                    found.pattern_given = true;
                } else if Some(flag) == file_short {
                    found.pattern_given = true;
                    found.value_paths.extend(value);
                }
                break;
            }
            continue;
        }
        found.paths.push(arg.clone());
    }
    found
}

/// `find [-H|-L|-P] [-D opts] [-Olevel] ROOT... EXPRESSION`: the roots
/// stop at the first word that starts an expression.
fn find_roots(args: &[String]) -> Vec<String> {
    let mut words = args.iter().peekable();
    while let Some(arg) = words.peek() {
        match arg.as_str() {
            "-H" | "-L" | "-P" => {
                words.next();
            }
            "-D" => {
                words.next();
                words.next();
            }
            arg if arg.starts_with("-O") => {
                words.next();
            }
            _ => break,
        }
    }
    words
        .take_while(|arg| !arg.starts_with('-') && !matches!(arg.as_str(), "(" | ")" | "!"))
        .cloned()
        .collect()
}

/// Git's operands, read wide: its grammar differs per subcommand, so every
/// word that is not an option is kept, and the path half of a
/// `<rev>:<path>` too, which is how `git show` reads a file.
fn git_paths(args: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut words = args.iter();
    let mut subcommand_seen = false;
    while let Some(arg) = words.next() {
        if !subcommand_seen {
            match arg.as_str() {
                "-C" => {
                    paths.extend(words.next().cloned());
                    continue;
                }
                arg if arg.starts_with('-') => continue,
                _ => {
                    subcommand_seen = true;
                    continue;
                }
            }
        }
        if arg.starts_with('-') && arg != "-" {
            continue;
        }
        if let Some((_, path)) = arg.split_once(':') {
            if !path.is_empty() {
                paths.push(path.to_string());
            }
        }
        paths.push(arg.clone());
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(command: &str) -> Vec<String> {
        read_paths(
            &command
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn operands_are_paths_and_option_values_are_not() {
        assert_eq!(paths("cat a b"), ["a", "b"]);
        assert_eq!(paths("cat -n -- -odd"), ["-odd"]);
        assert_eq!(paths("cat -"), Vec::<String>::new());
        assert_eq!(paths("head -n 20 a"), ["a"]);
        assert_eq!(paths("head -n20 a"), ["a"]);
        assert_eq!(paths("tail --lines=5 a"), ["a"]);
        assert_eq!(paths("tail -f log"), ["log"]);
        assert_eq!(paths("wc -l a b"), ["a", "b"]);
        assert_eq!(paths("ls -la src"), ["src"]);
        assert_eq!(paths("ls"), Vec::<String>::new());
        assert_eq!(paths("cut -d , -f 1 a.csv"), ["a.csv"]);
        assert_eq!(paths("sort -k 2 a"), ["a"]);
    }

    #[test]
    fn a_pattern_or_script_is_not_a_path() {
        assert_eq!(paths("grep -rn TODO crates"), ["crates"]);
        assert_eq!(paths("grep -A 3 TODO a b"), ["a", "b"]);
        assert_eq!(paths("grep -e TODO a"), ["a"]);
        assert_eq!(paths("grep -rne TODO a"), ["a"]);
        assert_eq!(paths("grep -f patterns.txt a"), ["a", "patterns.txt"]);
        assert_eq!(paths("rg secrets src"), ["src"]);
        assert_eq!(paths("rg -n -g *.rs foo src"), ["src"]);
        assert_eq!(paths("rg --files secrets"), ["secrets"]);
        assert_eq!(paths("rg foo"), Vec::<String>::new());
        assert_eq!(paths("sed -n 1,5p a"), ["a"]);
        assert_eq!(paths("sed -e 1p -e 2p a b"), ["a", "b"]);
        assert_eq!(paths("fd lexer crates"), ["crates"]);
        assert_eq!(paths("fd -e rs lexer"), Vec::<String>::new());
        assert_eq!(paths("fd --search-path secrets x"), ["secrets"]);
    }

    #[test]
    fn find_roots_and_git_operands() {
        assert_eq!(paths("find secrets -name x"), ["secrets"]);
        assert_eq!(paths("find -L a b -type f"), ["a", "b"]);
        assert_eq!(paths("find . ( -name a )"), ["."]);
        assert_eq!(paths("find -name x"), Vec::<String>::new());
        assert_eq!(
            paths("git show HEAD:secrets/x"),
            ["secrets/x", "HEAD:secrets/x"]
        );
        assert_eq!(paths("git -C secrets log -1"), ["secrets"]);
        assert_eq!(paths("git diff HEAD -- src"), ["HEAD", "src"]);
        assert_eq!(paths("git status --short"), Vec::<String>::new());
        assert_eq!(paths("echo secrets/x"), Vec::<String>::new());
        assert_eq!(paths("pwd"), Vec::<String>::new());
    }
}
