//! POSIX shell quoting.
//!
//! Every remote command Rebon builds is handed to `ssh`, and `ssh`
//! does not take an argv — it joins whatever it is given with spaces
//! and feeds the result to the *remote login shell*. So a path with a
//! space, a `$`, or a quote is not an escaping nicety here: it is the
//! difference between running one command and running two.
//!
//! Single-quoting is used rather than backslash-escaping because it is
//! total: inside `'…'` a POSIX shell expands nothing at all. The one
//! character that cannot appear there is `'` itself, which is closed,
//! escaped, and reopened.

/// Quote a string so a POSIX shell reproduces it verbatim as one word.
pub fn sh_quote(value: &str) -> String {
    // The empty string still needs quotes, or it vanishes from argv.
    if value.is_empty() {
        return "''".to_string();
    }
    // Fast path: nothing a shell would look at twice. Kept because
    // remote command lines end up in error messages and logs, and
    // `'/home/me/.rebon/server/0.15.0/rebon'` reads worse than the
    // bare path when there was never anything to quote.
    if value.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'+' | b',')
    }) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Join pre-built words into one remote command line.
pub fn sh_join<I, S>(words: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    words
        .into_iter()
        .map(|word| sh_quote(word.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_are_left_alone() {
        assert_eq!(sh_quote("rebon"), "rebon");
        assert_eq!(sh_quote("/srv/app/rebon"), "/srv/app/rebon");
        assert_eq!(sh_quote("--acp"), "--acp");
        assert_eq!(sh_quote("0.15.0"), "0.15.0");
    }

    #[test]
    fn empty_string_survives_as_an_argument() {
        // Without quotes this word disappears and every later
        // argument shifts left by one.
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn spaces_and_metacharacters_are_neutralised() {
        assert_eq!(sh_quote("/srv/my app"), "'/srv/my app'");
        assert_eq!(sh_quote("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(sh_quote("$HOME"), "'$HOME'");
        assert_eq!(sh_quote("`whoami`"), "'`whoami`'");
        assert_eq!(sh_quote("a&&b"), "'a&&b'");
        assert_eq!(sh_quote("*"), "'*'");
        assert_eq!(sh_quote("~/project"), "'~/project'");
    }

    #[test]
    fn embedded_single_quotes_close_and_reopen() {
        assert_eq!(sh_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(sh_quote("'"), r#"''\'''"#);
    }

    #[test]
    fn newlines_stay_inside_one_word() {
        assert_eq!(sh_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn join_quotes_each_word_independently() {
        assert_eq!(
            sh_join(["/srv/my app/rebon", "--acp"]),
            "'/srv/my app/rebon' --acp"
        );
    }

    #[test]
    fn a_quoted_word_cannot_smuggle_a_second_command() {
        // The whole point of the module: a hostile project path is one
        // word to the remote shell, not a command separator.
        let line = sh_join(["cd", "/tmp'; rm -rf ~; echo '"]);
        assert_eq!(line, r#"cd '/tmp'\''; rm -rf ~; echo '\'''"#);
        assert!(!line.contains("; rm -rf ~; echo ;"));
    }
}
