//! The confined process's environment.
//!
//! The contract: the child gets `--inherit-env` names, plus `--env` pairs,
//! minus `--unset-env` names, and **nothing else from the caller**. The caller
//! is a Rebon process holding every API key the user has configured; handing it
//! its parent's environment would carry all of them across.
//!
//! ## Where the rest of the environment comes from
//!
//! A process needs more than `PATH` to run — `SystemRoot`, `ComSpec`, `TEMP`
//! and friends — and it cannot get them from the caller under that rule. It
//! gets them from the *sandbox account's own* profile block, which
//! `CreateEnvironmentBlock` builds from the logon token. That base is the
//! sandbox user's environment, so nothing in it belongs to the caller, and
//! [`EnvBlock::apply`] layers the request on top of it.
//!
//! ## Case
//!
//! Windows environment names are case-insensitive, so `Path` and `PATH` are one
//! variable. A plain map would let them coexist and the child would receive a
//! block with a duplicate — not an error, just a race over whichever spelling
//! the runtime happens to read first. [`EnvBlock`] keys on the upper-cased name
//! and keeps the original for display, which is also why `--unset-env no_proxy`
//! correctly removes an inherited `NO_PROXY`.
//!
//! The block Windows wants is sorted case-insensitively by name, which is what
//! iterating the keyed map gives for free.

use crate::core::argv::ExecRequest;
use std::collections::BTreeMap;

/// A case-insensitive environment, in the order Windows wants it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvBlock {
    /// Upper-cased name mapped to the spelling as set and the value.
    entries: BTreeMap<String, (String, String)>,
}

impl EnvBlock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: &str, value: &str) {
        self.entries
            .insert(name.to_uppercase(), (name.to_string(), value.to_string()));
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(&name.to_uppercase()).is_some()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .get(&name.to_uppercase())
            .map(|(_, value)| value.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(&name.to_uppercase())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Name/value pairs, sorted case-insensitively by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .values()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// The `NAME=VALUE` strings a Win32 environment block is built from.
    pub fn to_pairs(&self) -> Vec<String> {
        self.iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect()
    }

    /// Apply one request's environment rules on top of this block.
    ///
    /// Order is `inherit` → `set` → `unset`, so an explicit `--unset-env` wins both
    /// over an inherited value and over a `--env` naming the same variable. That is
    /// the only order in which "unset this" means what it says.
    pub fn apply<F>(&mut self, request: &ExecRequest, from_caller: F)
    where
        F: Fn(&str) -> Option<String>,
    {
        for name in &request.inherit_env {
            if let Some(value) = from_caller(name) {
                self.set(name, &value);
            }
        }
        for (name, value) in &request.set_env {
            self.set(name, value);
        }
        for name in &request.unset_env {
            self.remove(name);
        }
    }
}

impl FromIterator<(String, String)> for EnvBlock {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        let mut block = EnvBlock::new();
        for (name, value) in iter {
            block.set(&name, &value);
        }
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ExecRequest {
        ExecRequest {
            command: vec!["cmd.exe".into()],
            ..Default::default()
        }
    }

    fn base() -> EnvBlock {
        [
            ("SystemRoot".to_string(), r"C:\Windows".to_string()),
            (
                "ComSpec".to_string(),
                r"C:\Windows\system32\cmd.exe".to_string(),
            ),
            ("USERNAME".to_string(), "rebon-sbx".to_string()),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn the_sandbox_users_own_profile_survives_untouched() {
        let mut block = base();
        block.apply(&request(), |_| None);

        assert_eq!(block.get("SystemRoot"), Some(r"C:\Windows"));
        assert_eq!(block.get("USERNAME"), Some("rebon-sbx"));
    }

    #[test]
    fn only_the_named_variables_come_from_the_caller() {
        // The caller holds the user's API keys. Anything not named here must not
        // cross, and the closure is the only door.
        let mut request = request();
        request.inherit_env = vec!["PATH".into(), "PATHEXT".into()];
        let asked = std::cell::RefCell::new(Vec::new());

        let mut block = base();
        block.apply(&request, |name| {
            asked.borrow_mut().push(name.to_string());
            Some(format!("value-of-{name}"))
        });

        assert_eq!(asked.into_inner(), vec!["PATH", "PATHEXT"]);
        assert_eq!(block.get("PATH"), Some("value-of-PATH"));
        assert!(!block.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn a_variable_the_caller_does_not_have_is_simply_absent() {
        let mut request = request();
        request.inherit_env = vec!["PATHEXT".into()];

        let mut block = EnvBlock::new();
        block.apply(&request, |_| None);

        assert!(!block.contains("PATHEXT"));
    }

    #[test]
    fn env_pairs_override_inherited_values() {
        let mut request = request();
        request.inherit_env = vec!["PATH".into()];
        request.set_env = vec![("PATH".into(), r"C:\only".into())];

        let mut block = EnvBlock::new();
        block.apply(&request, |_| Some(r"C:\caller".into()));

        assert_eq!(block.get("PATH"), Some(r"C:\only"));
    }

    #[test]
    fn unset_wins_over_both_inherit_and_env() {
        let mut request = request();
        request.inherit_env = vec!["HTTP_PROXY".into()];
        request.set_env = vec![("HTTP_PROXY".into(), "http://x".into())];
        request.unset_env = vec!["HTTP_PROXY".into()];

        let mut block = EnvBlock::new();
        block.apply(&request, |_| Some("http://caller".into()));

        assert!(!block.contains("HTTP_PROXY"));
    }

    #[test]
    fn unsetting_is_case_insensitive_like_windows() {
        // The caller emits both `no_proxy` and `NO_PROXY`; a case-sensitive map would
        // leave whichever spelling the profile happened to use, and a surviving
        // `no_proxy` is a hole straight past the proxy.
        let mut request = request();
        request.unset_env = vec!["no_proxy".into()];

        let mut block: EnvBlock = [("NO_PROXY".to_string(), "localhost".to_string())]
            .into_iter()
            .collect();
        block.apply(&request, |_| None);

        assert!(!block.contains("NO_PROXY"));
        assert!(block.is_empty());
    }

    #[test]
    fn one_variable_cannot_appear_twice_under_two_spellings() {
        let mut block = EnvBlock::new();
        block.set("Path", r"C:\a");
        block.set("PATH", r"C:\b");

        assert_eq!(block.len(), 1);
        assert_eq!(block.get("path"), Some(r"C:\b"));
        assert_eq!(block.to_pairs(), vec![r"PATH=C:\b".to_string()]);
    }

    #[test]
    fn the_block_is_sorted_case_insensitively() {
        // Windows requires this ordering in the block it is handed.
        let mut block = EnvBlock::new();
        block.set("zeta", "1");
        block.set("Alpha", "2");
        block.set("mid", "3");

        let names: Vec<&str> = block.iter().map(|(name, _)| name).collect();

        assert_eq!(names, vec!["Alpha", "mid", "zeta"]);
    }

    #[test]
    fn an_empty_value_is_kept_rather_than_dropped() {
        // `KEY=` is a legitimate setting and means something different from the
        // variable being absent.
        let mut request = request();
        request.set_env = vec![("EMPTY".into(), String::new())];

        let mut block = EnvBlock::new();
        block.apply(&request, |_| None);

        assert_eq!(block.get("EMPTY"), Some(""));
        assert_eq!(block.to_pairs(), vec!["EMPTY=".to_string()]);
    }

    #[test]
    fn removing_reports_whether_anything_went() {
        let mut block = base();
        assert!(block.remove("SYSTEMROOT"));
        assert!(!block.remove("SYSTEMROOT"));
    }

    #[test]
    fn git_config_pairs_from_the_caller_arrive_intact() {
        let mut request = request();
        request.set_env = vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "safe.directory".into()),
            ("GIT_CONFIG_VALUE_0".into(), r"C:\work".into()),
        ];

        let mut block = EnvBlock::new();
        block.apply(&request, |_| None);

        assert_eq!(block.get("GIT_CONFIG_VALUE_0"), Some(r"C:\work"));
        assert_eq!(block.len(), 3);
    }
}
