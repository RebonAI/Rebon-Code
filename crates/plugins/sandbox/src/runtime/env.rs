//! Environment the sandbox injects — RFC §5.2, §6.2, §8.
//!
//! Three groups, built here once and used by all three backends so a
//! Linux command and a Windows command cannot end up with different
//! proxy variables for the same session:
//!
//! * **proxy variables** — how a confined command reaches the network
//!   at all, since the kernel/WFP layer only lets loopback through;
//! * **git safe directories** — injected as `GIT_CONFIG_*` rather
//!   than written to the user's `~/.gitconfig`, because a sandbox
//!   must not leave state behind on the host;
//! * **compatibility variables** — the handful of "this runtime needs
//!   one more hint to use a proxy" cases, each tied to the condition
//!   that makes it necessary.

use crate::runtime::config::{EffectiveConfig, RuntimePaths};
use std::path::Path;

/// Variables that must be removed for the proxy to be the only way
/// out.
///
/// `NO_PROXY` / `no_proxy` are here because either one can name a
/// host that then bypasses the proxy entirely — with the proxy being
/// the thing that makes the domain-allowlist decision, an inherited
/// `no_proxy=*` would turn network restriction off without any error.
/// `TMPDIR` is removed because the host temp dir is usually outside
/// the sandbox's writable roots, and inheriting it produces confusing
/// permission failures deep inside unrelated tools.
pub const PROXY_STRIPPED_VARS: &[&str] = &["NO_PROXY", "no_proxy", "TMPDIR"];

/// The env pairs a confined command needs to reach the network
/// through the session's loopback proxy.
///
/// Returns an empty vector when the session has no proxy configured:
/// there is nothing to point at, and emitting `http_proxy` for a port
/// nobody listens on would turn "no network" into "every request
/// fails with a confusing connection-refused".
pub fn proxy_env(runtime: &RuntimePaths) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(port) = runtime.http_proxy_port {
        let url = format!("http://127.0.0.1:{port}");
        // Both cases are set: curl reads the lowercase names, most
        // language runtimes read either, and a few (notably older
        // Java and some Go libraries) only read the uppercase ones.
        env.push(("http_proxy".to_string(), url.clone()));
        env.push(("HTTP_PROXY".to_string(), url.clone()));
        env.push(("https_proxy".to_string(), url.clone()));
        env.push(("HTTPS_PROXY".to_string(), url));
    }
    if let Some(port) = runtime.socks_proxy_port {
        let url = format!("socks5://127.0.0.1:{port}");
        env.push(("all_proxy".to_string(), url.clone()));
        env.push(("ALL_PROXY".to_string(), url));
    }
    if let Some(token) = &runtime.proxy_auth_token {
        env.push(("PROXY_AUTH_TOKEN".to_string(), token.clone()));
    }
    if let Some(cert) = &runtime.proxy_ca_cert_path {
        let cert = cert.to_string_lossy().into_owned();
        // One certificate, five names, because no two TLS stacks
        // agree: OpenSSL/curl, Node, Python (requests/certifi), Go,
        // and Deno each read a different variable.
        env.push(("SSL_CERT_FILE".to_string(), cert.clone()));
        env.push(("NODE_EXTRA_CA_CERTS".to_string(), cert.clone()));
        env.push(("REQUESTS_CA_BUNDLE".to_string(), cert.clone()));
        env.push(("CURL_CA_BUNDLE".to_string(), cert.clone()));
        env.push(("DENO_CERT".to_string(), cert));
    }
    if let Some(tmp) = &runtime.sandbox_tmp_dir {
        let tmp = tmp.to_string_lossy().into_owned();
        env.push(("TMPDIR".to_string(), tmp.clone()));
        env.push(("REBON_TMPDIR".to_string(), tmp));
    }
    env
}

/// `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` for
/// the directories git should treat as safe — RFC §8.
///
/// Injected as environment rather than written to a config file for
/// two reasons: the sandbox must not mutate host state, and the
/// alternative failure (`detected dubious ownership`) makes git print
/// a remediation hint that tells the user to run a command that would
/// permanently widen their real git config.
///
/// Returns an empty vector for an empty input — emitting
/// `GIT_CONFIG_COUNT=0` is harmless but noisy, and an empty list of
/// safe directories is the common case.
pub fn git_safe_directory_env(dirs: &[impl AsRef<Path>]) -> Vec<(String, String)> {
    if dirs.is_empty() {
        return Vec::new();
    }
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), dirs.len().to_string())];
    for (index, dir) in dirs.iter().enumerate() {
        env.push((
            format!("GIT_CONFIG_KEY_{index}"),
            "safe.directory".to_string(),
        ));
        env.push((
            format!("GIT_CONFIG_VALUE_{index}"),
            dir.as_ref().to_string_lossy().into_owned(),
        ));
    }
    env
}

/// Extra variables a runtime needs before it will use the proxy.
///
/// Currently one entry, and it is gated twice on purpose: Java only
/// needs the IPv4 hint when it is going to talk to a loopback proxy
/// *and* the sandbox lets it bind locally, and setting
/// `JAVA_TOOL_OPTIONS` unconditionally makes every JVM in every
/// sandboxed command print a banner line to stderr.
pub fn compatibility_env(config: &EffectiveConfig) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if config.network_restricted && config.network.allow_local_binding {
        env.push((
            "JAVA_TOOL_OPTIONS".to_string(),
            "-Djava.net.preferIPv4Stack=true".to_string(),
        ));
    }
    env
}

/// The complete, ordered environment mutation for one command.
///
/// Order is the contract: later entries overwrite earlier ones, and
/// callers apply `unset` after `set`. Credential rules were already
/// folded into `config.env_set` / `config.env_unset` by
/// [`EffectiveConfig::merge`](crate::runtime::config::EffectiveConfig::merge),
/// and they come *last* here so nothing built in this module can
/// reintroduce a masked credential.
pub struct EnvPlan {
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
}

/// Build the environment plan for a command.
pub fn build_env_plan(config: &EffectiveConfig) -> EnvPlan {
    let mut set: Vec<(String, String)> = Vec::new();
    let mut unset: Vec<String> = Vec::new();

    if config.network_restricted {
        for var in PROXY_STRIPPED_VARS {
            unset.push((*var).to_string());
        }
        set.extend(proxy_env(&config.runtime));
        set.extend(compatibility_env(config));
    } else if let Some(tmp) = &config.runtime.sandbox_tmp_dir {
        // An unrestricted command still gets the sandbox temp dir if
        // one is configured: it is inside the writable roots, and the
        // host temp dir may not be.
        set.push(("TMPDIR".to_string(), tmp.to_string_lossy().into_owned()));
    }

    set.extend(git_safe_directory_env(&config.git_safe_directories));

    // Credential and caller rules last — see the doc comment.
    for (key, value) in &config.env_set {
        set.retain(|(existing, _)| existing != key);
        set.push((key.clone(), value.clone()));
    }
    for key in &config.env_unset {
        set.retain(|(existing, _)| existing != key);
        if !unset.contains(key) {
            unset.push(key.clone());
        }
    }
    // A variable that is both set and unset is a contradiction; the
    // set wins, because the only way a key lands in `set` this late
    // is an explicit credential mask.
    unset.retain(|key| !set.iter().any(|(existing, _)| existing == key));

    EnvPlan { set, unset }
}

/// Point the tools that take one at a masked credential's fake file.
///
/// For the platforms where a mask cannot be a filesystem redirect — macOS and
/// Windows, where it degrades to a plain denial. See
/// [`crate::runtime::mask_redirect`] for why this exists and why it cannot make things
/// worse: the denial is applied either way, so a tool that ignores the
/// variable is refused exactly as it would have been.
///
/// **Never overrides a variable the caller already decided.** If the caller
/// set or unset it themselves, that is a statement about their own
/// environment, and the mask still denies the real file — so honouring it
/// costs nothing and leaks nothing.
///
/// Returns the variables it set, for the warning text.
pub fn apply_mask_redirects(plan: &mut EnvPlan, config: &EffectiveConfig) -> Vec<String> {
    let mut applied = Vec::new();
    for redirect in crate::runtime::mask_redirect::redirects_for(&config.masked_files) {
        let decided = plan.set.iter().any(|(key, _)| key == &redirect.variable)
            || plan.unset.iter().any(|key| key == &redirect.variable);
        if decided {
            continue;
        }
        plan.set.push((redirect.variable.clone(), redirect.value));
        applied.push(redirect.variable);
    }
    applied
}

/// Whether a mask on `real` will be served through an environment variable.
///
/// The backends ask so their degradation warning can say which of the two
/// things happened, rather than one sentence that is half wrong.
pub fn mask_redirect_variable(bind: &crate::runtime::config::MaskedFileBind) -> Option<String> {
    crate::runtime::mask_redirect::redirect_for(bind).map(|redirect| redirect.variable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::config::{
        BinShell, CommandRequest, CredentialEnvRule, SessionSandboxConfig,
    };
    use std::path::PathBuf;

    fn config_with(
        mutate: impl FnOnce(&mut SessionSandboxConfig),
        restricted: bool,
    ) -> EffectiveConfig {
        let mut session = SessionSandboxConfig::default();
        mutate(&mut session);
        let request =
            CommandRequest::new("echo hi", BinShell::posix()).with_network_restriction(restricted);
        EffectiveConfig::merge(&session, &request)
    }

    fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn masked(real: &str, fake: &str) -> EffectiveConfig {
        config_with(
            |session| {
                session.credentials.files.push((
                    PathBuf::from(real),
                    crate::runtime::config::CredentialFileRule::Mask {
                        fake: PathBuf::from(fake),
                    },
                ));
            },
            false,
        )
    }

    #[test]
    fn a_masked_dotfile_gets_its_tools_variable() {
        let config = masked("/Users/me/.npmrc", "/tmp/fake-npmrc");
        let mut plan = build_env_plan(&config);
        let applied = apply_mask_redirects(&mut plan, &config);

        assert_eq!(applied, vec!["NPM_CONFIG_USERCONFIG".to_string()]);
        assert_eq!(
            get(&plan.set, "NPM_CONFIG_USERCONFIG"),
            Some("/tmp/fake-npmrc")
        );
    }

    #[test]
    fn a_masked_file_no_tool_knows_gets_nothing() {
        // It stays denied — which is what it was going to be anyway. The
        // table only ever upgrades a denial into a served fake.
        let config = masked("/Users/me/.some-secret", "/tmp/fake");
        let mut plan = build_env_plan(&config);
        assert!(apply_mask_redirects(&mut plan, &config).is_empty());
    }

    #[test]
    fn a_variable_the_caller_already_set_is_left_alone() {
        // The caller's own environment is their statement about it, and the
        // real file is denied either way — so honouring it leaks nothing.
        let mut config = masked("/Users/me/.npmrc", "/tmp/fake-npmrc");
        config
            .env_set
            .insert("NPM_CONFIG_USERCONFIG".into(), "/opt/mine".into());
        let mut plan = build_env_plan(&config);

        assert!(apply_mask_redirects(&mut plan, &config).is_empty());
        assert_eq!(get(&plan.set, "NPM_CONFIG_USERCONFIG"), Some("/opt/mine"));
    }

    #[test]
    fn a_variable_the_caller_unset_stays_unset() {
        let mut config = masked("/Users/me/.npmrc", "/tmp/fake-npmrc");
        config.env_unset.push("NPM_CONFIG_USERCONFIG".into());
        let mut plan = build_env_plan(&config);

        assert!(apply_mask_redirects(&mut plan, &config).is_empty());
        assert!(get(&plan.set, "NPM_CONFIG_USERCONFIG").is_none());
        assert!(plan.unset.iter().any(|k| k == "NPM_CONFIG_USERCONFIG"));
    }

    #[test]
    fn applying_twice_does_not_duplicate_the_variable() {
        // A duplicate key in an environment block is not an error; it just
        // means whichever the runtime reads first wins, silently.
        let config = masked("/Users/me/.gitconfig", "/tmp/fake-gitconfig");
        let mut plan = build_env_plan(&config);
        apply_mask_redirects(&mut plan, &config);
        apply_mask_redirects(&mut plan, &config);

        let count = plan
            .set
            .iter()
            .filter(|(key, _)| key == "GIT_CONFIG_GLOBAL")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn no_proxy_configured_yields_no_proxy_env() {
        assert!(proxy_env(&RuntimePaths::default()).is_empty());
    }

    #[test]
    fn the_running_proxy_needs_no_certificate_and_no_token() {
        // Rebon's proxy does not intercept TLS, so a session it starts sets
        // neither. Pinned because the opposite — emitting `SSL_CERT_FILE` for
        // a bundle that does not exist — would make every HTTPS request in
        // the sandbox fail to verify, which is a much louder failure than the
        // one it would be trying to prevent.
        let runtime = RuntimePaths {
            http_proxy_port: Some(3128),
            socks_proxy_port: Some(1080),
            ..Default::default()
        };

        let env = proxy_env(&runtime);

        for absent in [
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
            "DENO_CERT",
            "PROXY_AUTH_TOKEN",
        ] {
            assert!(get(&env, absent).is_none(), "{absent} was set");
        }
        assert!(get(&env, "http_proxy").is_some());
    }

    #[test]
    fn http_proxy_port_sets_both_cases_and_both_schemes() {
        let runtime = RuntimePaths {
            http_proxy_port: Some(3128),
            ..Default::default()
        };
        let env = proxy_env(&runtime);
        for key in ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"] {
            assert_eq!(get(&env, key), Some("http://127.0.0.1:3128"), "{key}");
        }
    }

    #[test]
    fn socks_port_sets_all_proxy_only() {
        let runtime = RuntimePaths {
            socks_proxy_port: Some(1080),
            ..Default::default()
        };
        let env = proxy_env(&runtime);
        assert_eq!(get(&env, "all_proxy"), Some("socks5://127.0.0.1:1080"));
        assert!(get(&env, "http_proxy").is_none());
    }

    #[test]
    fn ca_cert_reaches_every_tls_stack_variable() {
        let runtime = RuntimePaths {
            proxy_ca_cert_path: Some(PathBuf::from("/tmp/ca.pem")),
            ..Default::default()
        };
        let env = proxy_env(&runtime);
        for key in [
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
            "DENO_CERT",
        ] {
            assert_eq!(get(&env, key), Some("/tmp/ca.pem"), "{key}");
        }
    }

    #[test]
    fn git_safe_directory_env_is_empty_for_no_directories() {
        let empty: [PathBuf; 0] = [];
        assert!(git_safe_directory_env(&empty).is_empty());
    }

    #[test]
    fn git_safe_directory_env_indexes_from_zero() {
        let env = git_safe_directory_env(&[PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(get(&env, "GIT_CONFIG_COUNT"), Some("2"));
        assert_eq!(get(&env, "GIT_CONFIG_KEY_0"), Some("safe.directory"));
        assert_eq!(get(&env, "GIT_CONFIG_VALUE_0"), Some("/a"));
        assert_eq!(get(&env, "GIT_CONFIG_KEY_1"), Some("safe.directory"));
        assert_eq!(get(&env, "GIT_CONFIG_VALUE_1"), Some("/b"));
    }

    #[test]
    fn restricted_network_strips_the_bypass_variables() {
        let plan = build_env_plan(&config_with(|_| {}, true));
        for var in PROXY_STRIPPED_VARS {
            assert!(plan.unset.contains(&(*var).to_string()), "{var}");
        }
    }

    #[test]
    fn unrestricted_network_does_not_strip_no_proxy() {
        let plan = build_env_plan(&config_with(|_| {}, false));
        assert!(!plan.unset.contains(&"no_proxy".to_string()));
    }

    #[test]
    fn sandbox_tmpdir_survives_the_tmpdir_strip() {
        let plan = build_env_plan(&config_with(
            |session| {
                session.runtime.sandbox_tmp_dir = Some(PathBuf::from("/sbx/tmp"));
            },
            true,
        ));
        assert_eq!(get(&plan.set, "TMPDIR"), Some("/sbx/tmp"));
        assert!(
            !plan.unset.contains(&"TMPDIR".to_string()),
            "a variable that is set must not also be unset"
        );
    }

    #[test]
    fn java_hint_needs_both_restriction_and_local_binding() {
        let without = config_with(|session| session.network.allow_local_binding = true, false);
        assert!(compatibility_env(&without).is_empty());

        let with = config_with(|session| session.network.allow_local_binding = true, true);
        assert_eq!(
            get(&compatibility_env(&with), "JAVA_TOOL_OPTIONS"),
            Some("-Djava.net.preferIPv4Stack=true")
        );

        let no_binding = config_with(|_| {}, true);
        assert!(compatibility_env(&no_binding).is_empty());
    }

    #[test]
    fn masked_credential_cannot_be_overwritten_by_proxy_env() {
        let plan = build_env_plan(&config_with(
            |session| {
                session.runtime.http_proxy_port = Some(3128);
                session.credentials.env_vars = vec![(
                    "http_proxy".into(),
                    CredentialEnvRule::Mask {
                        value: "http://blocked".into(),
                    },
                )];
            },
            true,
        ));
        assert_eq!(get(&plan.set, "http_proxy"), Some("http://blocked"));
        assert_eq!(
            plan.set.iter().filter(|(k, _)| k == "http_proxy").count(),
            1,
            "the overwritten pair must be removed, not shadowed"
        );
    }

    #[test]
    fn denied_credential_is_unset_even_when_the_proxy_would_set_it() {
        let plan = build_env_plan(&config_with(
            |session| {
                session.runtime.proxy_auth_token = Some("t".into());
                session.credentials.env_vars =
                    vec![("PROXY_AUTH_TOKEN".into(), CredentialEnvRule::Deny)];
            },
            true,
        ));
        assert!(get(&plan.set, "PROXY_AUTH_TOKEN").is_none());
        assert!(plan.unset.contains(&"PROXY_AUTH_TOKEN".to_string()));
    }
}
