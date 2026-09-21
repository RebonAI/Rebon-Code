//! The sandbox configuration model — RFC §3.
//!
//! Two shapes live here and they are deliberately different:
//!
//! * [`SessionSandboxConfig`] is compiled **once** per session by
//!   `initialize()`. It carries everything that is expensive to
//!   resolve (native binary paths, proxy ports, credential rules) or
//!   that a platform can only apply session-wide (Windows ACLs).
//! * [`CommandRequest`] is built **per command**. It carries only the
//!   things a single command may narrow further.
//!
//! [`EffectiveConfig`] is the merge of the two, and it is the only
//! thing the three backends ever read. Keeping the merge in one place
//! is what makes "deny wins over allow" a single rule instead of
//! three platform re-implementations that can drift apart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// RFC §3.1 — the session-level configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSandboxConfig {
    pub filesystem: FilesystemConfig,
    pub network: NetworkConfig,
    pub credentials: CredentialsConfig,
    pub misc: MiscConfig,
    pub runtime: RuntimePaths,
}

/// Filesystem isolation rules.
///
/// The four lists form two nested pairs. Writes: `allow_write` opens
/// a root, `deny_write` punches holes back into it. Reads: everything
/// is readable, `deny_read` closes a subtree, `allow_read` reopens a
/// path inside a closed subtree. This nesting is why the backends
/// cannot simply concatenate the lists — see
/// [`EffectiveConfig::write_rules`] / [`EffectiveConfig::read_rules`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilesystemConfig {
    /// Turn filesystem isolation off entirely while leaving network
    /// and credential isolation in place.
    pub disabled: bool,
    /// Roots the command may write to. Empty means "no writes".
    pub allow_write: Vec<PathBuf>,
    /// Paths inside `allow_write` that stay read-only. Deny wins.
    pub deny_write: Vec<PathBuf>,
    /// Subtrees the command may not read at all.
    pub deny_read: Vec<PathBuf>,
    /// Paths inside a `deny_read` subtree that stay readable.
    pub allow_read: Vec<PathBuf>,
    /// Whether the global git config is readable. Off by default
    /// because it routinely holds credential helper configuration.
    pub allow_git_config: bool,
}

/// Network isolation rules.
///
/// `allowed_domains` / `denied_domains` only bind on macOS, where the
/// seatbelt profile can express them. Linux has no kernel-level
/// domain filter under bubblewrap, so it degrades to "block
/// everything, route through the loopback proxy" and the proxy makes
/// the domain decision — RFC §4.4. That asymmetry is documented
/// rather than hidden because a caller that assumes Linux enforces
/// `deniedDomains` in the kernel would be wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkConfig {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
    pub allow_unix_sockets: Vec<PathBuf>,
    pub allow_all_unix_sockets: bool,
    /// Let the command bind and accept on local ports (dev servers).
    pub allow_local_binding: bool,
    /// Extra macOS mach services to allow by name; a trailing `*`
    /// becomes a `global-name-prefix` match.
    pub allow_mach_lookup: Vec<String>,
}

impl NetworkConfig {
    /// Whether any network rule is configured at all. Used by the
    /// fast path: an unrestricted network needs no wrapping.
    pub fn is_restricted(&self) -> bool {
        !self.allowed_domains.is_empty()
            || !self.denied_domains.is_empty()
            || !self.allow_unix_sockets.is_empty()
    }
}

/// What to do with one credential file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialFileRule {
    /// Make the path unreadable.
    Deny,
    /// Show different contents at that path. Linux does this with a
    /// read-only bind of `fake` over the real path; macOS cannot and
    /// degrades to [`CredentialFileRule::Deny`] (RFC §5.4).
    Mask { fake: PathBuf },
}

/// What to do with one credential environment variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialEnvRule {
    /// Remove the variable from the child environment.
    Deny,
    /// Replace its value with a placeholder, so a program that only
    /// checks for presence still takes its "configured" branch.
    Mask { value: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialsConfig {
    pub files: Vec<(PathBuf, CredentialFileRule)>,
    pub env_vars: Vec<(String, CredentialEnvRule)>,
}

impl CredentialsConfig {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.env_vars.is_empty()
    }
}

/// Platform capability switches that trade isolation for
/// compatibility. Each one is named `enable_weaker_*` when it
/// genuinely lowers the wall, so a config review can grep for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MiscConfig {
    /// Allow the command a pseudo-terminal (needed by anything that
    /// checks `isatty`, e.g. interactive-ish CLIs).
    pub allow_pty: bool,
    /// macOS: allow AppleEvents so `open` works (RFC §5.1).
    pub allow_apple_events: bool,
    /// macOS: allow `trustd` so Go's TLS stack can verify the proxy
    /// certificate. Lowers isolation — RFC §5.1.
    pub enable_weaker_network_isolation: bool,
    /// Linux: forward the proxy into a nested sandbox, so a sandbox
    /// started *inside* the sandbox still has network. Explicitly
    /// lowers isolation — RFC §4.3.
    pub enable_weaker_nested_sandbox: bool,
}

/// Where the native pieces live and which loopback ports the proxy
/// listens on. Resolved by `initialize()`; the backends only read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimePaths {
    pub bwrap_path: Option<PathBuf>,
    pub socat_path: Option<PathBuf>,
    pub sandbox_win_path: Option<PathBuf>,
    pub seccomp_config: Option<PathBuf>,
    pub http_proxy_port: Option<u16>,
    pub socks_proxy_port: Option<u16>,
    /// Unix socket the HTTP proxy accepts on.
    ///
    /// Linux needs this and not just the port: under
    /// `--unshare-net` the sandbox gets a *fresh* loopback, so
    /// `127.0.0.1:<http_proxy_port>` inside it is not the host's
    /// listener. A socket file, bind-mounted in, crosses the network
    /// namespace where a port cannot — see [`crate::runtime::linux::bridge`].
    pub http_proxy_socket: Option<PathBuf>,
    /// Unix socket the SOCKS proxy accepts on. See
    /// `http_proxy_socket`.
    pub socks_proxy_socket: Option<PathBuf>,
    /// Bearer token to inject as `PROXY_AUTH_TOKEN`.
    ///
    /// **Nothing in Rebon sets this, and Rebon's own proxy does not require
    /// it.** [`crate::proxy`] listens on loopback (or a socket file only
    /// this session's sandbox can reach) and grants exactly the configured
    /// allowlist, so an unauthenticated listener is not an escalation — and
    /// no standard HTTP client reads this variable anyway, so enforcing it
    /// would break every client rather than authenticate one.
    ///
    /// Left in place for a caller that supplies its own proxy and whose
    /// tooling knows the name.
    pub proxy_auth_token: Option<String>,
    /// CA bundle to point the common TLS stacks at.
    ///
    /// **Nothing in Rebon sets this.** Rebon's proxy does not intercept TLS:
    /// a `CONNECT` names its target in the request line, which is all a
    /// hostname decision needs, so there is no certificate to trust and no
    /// root for the user to install. See `crate::proxy` for why that
    /// was chosen over interception.
    ///
    /// The field and its five-variable env mapping stay because *which* five
    /// names to set is hard-won and would have to be rediscovered: OpenSSL
    /// and curl, Node, Python, Go, and Deno each read a different one. Set it
    /// only if you are running an intercepting proxy of your own.
    pub proxy_ca_cert_path: Option<PathBuf>,
    /// Writable temp directory handed to the sandbox as `TMPDIR`.
    pub sandbox_tmp_dir: Option<PathBuf>,
}

/// The shell that will actually run the command, as an argv prefix.
///
/// This is the type that makes the PowerShell RFC's §7.3 third row
/// expressible: on Windows the sandbox wrapper needs the *whole*
/// `pwsh -NoProfile -NonInteractive -EncodedCommand` prefix as the
/// start of the argv it builds, not just a shell path, because it
/// spawns without a shell interpreter in front. Modelling the shell
/// as `program + leading args` covers `sh -lc`, `bash -c`,
/// `pwsh … -Command`, and `pwsh … -EncodedCommand` with one shape.
///
/// Defined in `rebon-tool` rather than here: the tools that build one are
/// there, they build one whether or not this plugin is loaded, and a shell
/// prefix is not a sandbox concept. This crate is the only thing that turns
/// one into a confined argv.
pub use rebon_tool::command_sandbox::BinShell;

/// Read rules in the shape the backends want: a set of closed
/// subtrees plus the exceptions punched back into them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadRules {
    pub deny_only: Vec<PathBuf>,
    pub allow_within_deny: Vec<PathBuf>,
}

impl ReadRules {
    pub fn is_empty(&self) -> bool {
        self.deny_only.is_empty() && self.allow_within_deny.is_empty()
    }
}

/// Write rules: open roots plus the holes punched back into them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteRules {
    pub allow_only: Vec<PathBuf>,
    pub deny_within_allow: Vec<PathBuf>,
}

impl WriteRules {
    pub fn is_empty(&self) -> bool {
        self.allow_only.is_empty() && self.deny_within_allow.is_empty()
    }
}

/// One credential file whose contents are replaced rather than hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskedFileBind {
    /// Path the command will open.
    pub real: PathBuf,
    /// File whose contents it will actually see.
    pub fake: PathBuf,
}

/// RFC §3.2 — the per-command configuration.
///
/// Everything here narrows the session config; nothing widens it. A
/// command cannot grant itself a write root the session did not
/// configure, which is what keeps the session config auditable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    /// The payload handed to `bin_shell` — a shell script for
    /// `sh -lc`, a base64 blob for `pwsh -EncodedCommand`.
    pub command: String,
    /// Correlates violation reports back to this command (RFC §5.3).
    pub command_id: String,
    /// The shell to run it with.
    pub bin_shell: BinShell,
    pub cwd: Option<PathBuf>,
    /// Whether this command should be cut off from the network.
    pub needs_network_restriction: bool,
    /// Extra read denials for this command only.
    pub read_config: ReadRules,
    /// Extra write scoping for this command only.
    pub write_config: WriteRules,
    pub unset_env_vars: Vec<String>,
    pub set_env_vars: Vec<(String, String)>,
    /// Directories to register as `safe.directory` so git does not
    /// refuse them for `dubious ownership` under the sandbox uid.
    pub git_safe_directories: Vec<PathBuf>,
    /// Windows only: per-command read allowances, which the ACL
    /// backend cannot honour. Present so the caller gets
    /// [`crate::runtime::SandboxError::PerExecAclUnsupported`] instead of
    /// silently losing the rule.
    pub allow_read: Vec<PathBuf>,
    /// Windows only: see `allow_read`.
    pub allow_write: Vec<PathBuf>,
}

impl CommandRequest {
    /// A request that asks for nothing beyond running `command` —
    /// the shape that takes the RFC §3.3 fast path.
    pub fn new(command: impl Into<String>, bin_shell: BinShell) -> Self {
        Self {
            command: command.into(),
            command_id: String::new(),
            bin_shell,
            cwd: None,
            needs_network_restriction: false,
            read_config: ReadRules::default(),
            write_config: WriteRules::default(),
            unset_env_vars: Vec::new(),
            set_env_vars: Vec::new(),
            git_safe_directories: Vec::new(),
            allow_read: Vec::new(),
            allow_write: Vec::new(),
        }
    }

    pub fn with_command_id(mut self, id: impl Into<String>) -> Self {
        self.command_id = id.into();
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_network_restriction(mut self, restricted: bool) -> Self {
        self.needs_network_restriction = restricted;
        self
    }

    pub fn with_write_rules(mut self, rules: WriteRules) -> Self {
        self.write_config = rules;
        self
    }

    pub fn with_read_rules(mut self, rules: ReadRules) -> Self {
        self.read_config = rules;
        self
    }

    pub fn with_git_safe_directories(
        mut self,
        dirs: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Self {
        self.git_safe_directories = dirs.into_iter().map(Into::into).collect();
        self
    }
}

/// The merged session + per-command view the backends consume.
///
/// Built by [`EffectiveConfig::merge`]. Nothing else constructs one,
/// so the merge rules below hold for every backend by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub command: String,
    pub command_id: String,
    pub bin_shell: BinShell,
    pub cwd: Option<PathBuf>,
    pub network_restricted: bool,
    /// Whether the command asked for nothing — see
    /// [`EffectiveConfig::is_unrestricted`]. Stored at merge time
    /// rather than recomputed, because the constant denials added
    /// afterwards would change the answer.
    unrestricted: bool,
    pub read_rules: ReadRules,
    pub write_rules: WriteRules,
    pub masked_files: Vec<MaskedFileBind>,
    pub env_set: BTreeMap<String, String>,
    pub env_unset: Vec<String>,
    pub git_safe_directories: Vec<PathBuf>,
    pub network: NetworkConfig,
    pub misc: MiscConfig,
    pub runtime: RuntimePaths,
    pub allow_git_config: bool,
    /// Per-command read allowances, carried through unmerged.
    ///
    /// These are *not* folded into `read_rules`: the Unix backends
    /// can honour them, the Windows one cannot, and a backend that
    /// cannot must be able to see that the caller asked. Folding
    /// would leave Windows quietly enforcing a different policy than
    /// the caller requested — see
    /// [`crate::runtime::SandboxError::PerExecAclUnsupported`].
    pub allow_read_overrides: Vec<PathBuf>,
    /// Per-command write allowances. See `allow_read_overrides`.
    pub allow_write_overrides: Vec<PathBuf>,
}

/// Read-deny that is applied on every command regardless of config.
///
/// `ssh_config.d` can redirect an outbound ssh through a
/// `ProxyCommand`, which is an arbitrary-command primitive that would
/// run *outside* whatever the sandbox thinks it is confining. RFC
/// §4.1 pins it as a constant denial rather than a default the caller
/// may forget to set.
pub const ALWAYS_DENY_READ: &[&str] = &["/etc/ssh/ssh_config.d"];

impl EffectiveConfig {
    /// Merge one command's request onto the session config.
    ///
    /// Merge rules, in the order they are applied:
    ///
    /// 1. **Filesystem disabled wins.** `filesystem.disabled` drops
    ///    every read and write rule, from both levels. It does not
    ///    touch network or credentials — those are separate walls.
    /// 2. **Write roots intersect, denials union.** The session's
    ///    `allow_write` is the outer bound; a command may narrow it
    ///    with its own `write_config.allow_only`, never widen it.
    ///    Denials from both levels apply.
    /// 3. **Read denials union, allowances only apply within them.**
    ///    A command cannot open a subtree the session closed unless
    ///    the session itself listed the exception.
    /// 4. **Credential env rules are applied last**, so a masked
    ///    credential cannot be un-masked by `set_env_vars`.
    pub fn merge(session: &SessionSandboxConfig, request: &CommandRequest) -> Self {
        let fs = &session.filesystem;

        let (read_rules, write_rules, masked_files) = if fs.disabled {
            (ReadRules::default(), WriteRules::default(), Vec::new())
        } else {
            let write_rules = WriteRules {
                allow_only: intersect_roots(&fs.allow_write, &request.write_config.allow_only),
                deny_within_allow: union_paths(
                    &fs.deny_write,
                    &request.write_config.deny_within_allow,
                ),
            };

            let mut deny_read = union_paths(&fs.deny_read, &request.read_config.deny_only);
            let mut masked_files = Vec::new();
            for (path, rule) in &session.credentials.files {
                match rule {
                    CredentialFileRule::Deny => {
                        if !deny_read.contains(path) {
                            deny_read.push(path.clone());
                        }
                    }
                    CredentialFileRule::Mask { fake } => masked_files.push(MaskedFileBind {
                        real: path.clone(),
                        fake: fake.clone(),
                    }),
                }
            }

            // A per-command allowance only counts if the session
            // already listed it. Anything else would let a single
            // command re-open a subtree the session closed.
            let allow_within_deny = fs
                .allow_read
                .iter()
                .cloned()
                .chain(
                    request
                        .read_config
                        .allow_within_deny
                        .iter()
                        .filter(|p| contains_path(&fs.allow_read, p))
                        .cloned(),
                )
                .collect::<Vec<_>>();

            (
                ReadRules {
                    deny_only: deny_read,
                    allow_within_deny: dedupe(allow_within_deny),
                },
                write_rules,
                masked_files,
            )
        };

        let mut env_set = BTreeMap::new();
        for (key, value) in &request.set_env_vars {
            env_set.insert(key.clone(), value.clone());
        }
        let mut env_unset = request.unset_env_vars.clone();
        for (key, rule) in &session.credentials.env_vars {
            match rule {
                CredentialEnvRule::Deny => {
                    env_set.remove(key);
                    if !env_unset.contains(key) {
                        env_unset.push(key.clone());
                    }
                }
                CredentialEnvRule::Mask { value } => {
                    env_unset.retain(|existing| existing != key);
                    env_set.insert(key.clone(), value.clone());
                }
            }
        }

        // Whether anything was actually asked for — decided *before*
        // the constant denials are added, and stored rather than
        // recomputed. A constant that every command carries must not
        // be what takes every command off the fast path; if it were,
        // enabling the sandbox would wrap `echo` on every machine and
        // §3.3 would be dead code.
        let unrestricted = !request.needs_network_restriction
            && read_rules.is_empty()
            && write_rules.is_empty()
            && masked_files.is_empty()
            && env_set.is_empty()
            && env_unset.is_empty()
            && request.git_safe_directories.is_empty();

        let mut read_rules = read_rules;
        if !unrestricted && !fs.disabled {
            for extra in ALWAYS_DENY_READ {
                let path = PathBuf::from(extra);
                if !read_rules.deny_only.contains(&path) {
                    read_rules.deny_only.push(path);
                }
            }
        }

        Self {
            command: request.command.clone(),
            command_id: request.command_id.clone(),
            bin_shell: request.bin_shell.clone(),
            cwd: request.cwd.clone(),
            network_restricted: request.needs_network_restriction,
            unrestricted,
            read_rules,
            write_rules,
            masked_files,
            env_set,
            env_unset,
            git_safe_directories: request.git_safe_directories.clone(),
            network: session.network.clone(),
            misc: session.misc.clone(),
            runtime: session.runtime.clone(),
            allow_git_config: fs.allow_git_config,
            allow_read_overrides: request.allow_read.clone(),
            allow_write_overrides: request.allow_write.clone(),
        }
    }

    /// RFC §3.3 — whether this command needs no sandbox at all.
    ///
    /// The fast path is not an optimisation detail, it is the reason
    /// sandboxing can be on by default: a command with no rules to
    /// apply is spawned exactly as it would have been with the
    /// sandbox off, so enabling the sandbox costs nothing on the
    /// commands that do not need it.
    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted
    }

    /// The write rules, minus anything that is not a concrete path.
    ///
    /// Returns the kept paths and the glob patterns that were
    /// dropped, so the Linux backend can report each drop rather
    /// than silently narrowing the sandbox — RFC §4.4.
    pub fn concrete_write_roots(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut kept = Vec::new();
        let mut globs = Vec::new();
        for path in &self.write_rules.allow_only {
            if is_glob(path) {
                globs.push(path.clone());
            } else {
                kept.push(path.clone());
            }
        }
        (kept, globs)
    }
}

/// Whether a configured path is a glob pattern rather than a literal
/// path. Only the three characters a mount target cannot contain are
/// treated as glob markers; a path with a `?` in a filename is rare
/// enough, and refusing it is the safe direction.
pub fn is_glob(path: &Path) -> bool {
    path.to_string_lossy()
        .chars()
        .any(|c| matches!(c, '*' | '?' | '['))
}

/// Session roots narrowed by per-command roots.
///
/// An empty per-command list means "no narrowing", not "no roots" —
/// most commands do not scope their writes and must still get the
/// session's roots. A per-command root that is not inside any session
/// root is dropped: a command may not widen its own write scope.
fn intersect_roots(session: &[PathBuf], per_command: &[PathBuf]) -> Vec<PathBuf> {
    if per_command.is_empty() {
        return dedupe(session.to_vec());
    }
    if session.is_empty() {
        // No session roots means writes were never opened; a command
        // asking for one cannot be the thing that opens it.
        return Vec::new();
    }
    dedupe(
        per_command
            .iter()
            .filter(|candidate| session.iter().any(|root| is_within(candidate, root)))
            .cloned()
            .collect(),
    )
}

fn union_paths(a: &[PathBuf], b: &[PathBuf]) -> Vec<PathBuf> {
    dedupe(a.iter().chain(b.iter()).cloned().collect())
}

fn dedupe(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = Vec::with_capacity(paths.len());
    for path in paths {
        if !seen.contains(&path) {
            seen.push(path);
        }
    }
    seen
}

fn contains_path(haystack: &[PathBuf], needle: &Path) -> bool {
    haystack.iter().any(|p| p == needle)
}

/// Whether `candidate` is `root` or lives under it.
///
/// Purely lexical — the caller is expected to have canonicalised
/// already, and the Linux backend re-checks with `realpath` before
/// binding anything (RFC §4.1) precisely because a lexical answer is
/// not enough on its own.
pub fn is_within(candidate: &Path, root: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_write_roots(roots: &[&str]) -> SessionSandboxConfig {
        SessionSandboxConfig {
            filesystem: FilesystemConfig {
                allow_write: roots.iter().map(PathBuf::from).collect(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn plain_request() -> CommandRequest {
        CommandRequest::new("echo hi", BinShell::posix())
    }

    #[test]
    fn bin_shell_argv_puts_the_command_last() {
        let shell = BinShell::new("pwsh", ["-NoProfile", "-EncodedCommand"]);
        assert_eq!(
            shell.argv("QQBBAA=="),
            vec!["pwsh", "-NoProfile", "-EncodedCommand", "QQBBAA=="]
        );
    }

    #[test]
    fn empty_command_config_takes_the_fast_path() {
        let effective = EffectiveConfig::merge(&SessionSandboxConfig::default(), &plain_request());
        // The constant read-deny must not, by itself, defeat the fast
        // path — otherwise every command on every machine would be
        // wrapped and the fast path would never fire.
        assert!(effective.is_unrestricted());
    }

    #[test]
    fn any_single_restriction_leaves_the_fast_path() {
        let cases: Vec<(&str, CommandRequest)> = vec![
            ("network", plain_request().with_network_restriction(true)),
            (
                "write",
                plain_request().with_write_rules(WriteRules {
                    allow_only: vec![PathBuf::from("/w")],
                    deny_within_allow: Vec::new(),
                }),
            ),
            (
                "read",
                plain_request().with_read_rules(ReadRules {
                    deny_only: vec![PathBuf::from("/secret")],
                    allow_within_deny: Vec::new(),
                }),
            ),
            (
                "git safe dirs",
                plain_request().with_git_safe_directories(["/repo"]),
            ),
            ("env set", {
                let mut request = plain_request();
                request.set_env_vars.push(("A".into(), "1".into()));
                request
            }),
            ("env unset", {
                let mut request = plain_request();
                request.unset_env_vars.push("A".into());
                request
            }),
        ];
        for (label, request) in cases {
            let session = session_with_write_roots(&["/w"]);
            let effective = EffectiveConfig::merge(&session, &request);
            assert!(
                !effective.is_unrestricted(),
                "{label} should have left the fast path"
            );
        }
    }

    #[test]
    fn always_deny_read_is_appended_once_even_if_configured() {
        let mut session = SessionSandboxConfig::default();
        session.filesystem.deny_read = vec![PathBuf::from("/etc/ssh/ssh_config.d")];
        let mut request = plain_request();
        request.read_config.deny_only = vec![PathBuf::from("/etc/ssh/ssh_config.d")];

        let effective = EffectiveConfig::merge(&session, &request);

        assert_eq!(
            effective
                .read_rules
                .deny_only
                .iter()
                .filter(|p| p.as_path() == Path::new("/etc/ssh/ssh_config.d"))
                .count(),
            1
        );
    }

    #[test]
    fn filesystem_disabled_drops_fs_rules_but_keeps_network() {
        let mut session = session_with_write_roots(&["/w"]);
        session.filesystem.disabled = true;
        session.filesystem.deny_read = vec![PathBuf::from("/secret")];
        let request = plain_request().with_network_restriction(true);

        let effective = EffectiveConfig::merge(&session, &request);

        assert!(effective.read_rules.is_empty());
        assert!(effective.write_rules.is_empty());
        assert!(effective.network_restricted);
        assert!(!effective.is_unrestricted());
    }

    #[test]
    fn per_command_write_root_inside_session_root_narrows() {
        let session = session_with_write_roots(&["/work"]);
        let request = plain_request().with_write_rules(WriteRules {
            allow_only: vec![PathBuf::from("/work/sub")],
            deny_within_allow: Vec::new(),
        });

        let effective = EffectiveConfig::merge(&session, &request);

        assert_eq!(
            effective.write_rules.allow_only,
            vec![PathBuf::from("/work/sub")]
        );
    }

    #[test]
    fn per_command_write_root_outside_session_root_is_dropped() {
        let session = session_with_write_roots(&["/work"]);
        let request = plain_request().with_write_rules(WriteRules {
            allow_only: vec![PathBuf::from("/etc")],
            deny_within_allow: Vec::new(),
        });

        let effective = EffectiveConfig::merge(&session, &request);

        assert!(
            effective.write_rules.allow_only.is_empty(),
            "a command must not widen its own write scope"
        );
    }

    #[test]
    fn command_cannot_open_a_subtree_the_session_closed() {
        let mut session = SessionSandboxConfig::default();
        session.filesystem.deny_read = vec![PathBuf::from("/secret")];
        let request = plain_request().with_read_rules(ReadRules {
            deny_only: Vec::new(),
            allow_within_deny: vec![PathBuf::from("/secret/key")],
        });

        let effective = EffectiveConfig::merge(&session, &request);

        assert!(effective.read_rules.allow_within_deny.is_empty());
    }

    #[test]
    fn command_may_reuse_a_session_declared_exception() {
        let mut session = SessionSandboxConfig::default();
        session.filesystem.deny_read = vec![PathBuf::from("/secret")];
        session.filesystem.allow_read = vec![PathBuf::from("/secret/public")];
        let request = plain_request().with_read_rules(ReadRules {
            deny_only: Vec::new(),
            allow_within_deny: vec![PathBuf::from("/secret/public")],
        });

        let effective = EffectiveConfig::merge(&session, &request);

        assert_eq!(
            effective.read_rules.allow_within_deny,
            vec![PathBuf::from("/secret/public")]
        );
    }

    #[test]
    fn credential_deny_env_wins_over_a_command_set() {
        let mut session = SessionSandboxConfig::default();
        session.credentials.env_vars = vec![("TOKEN".into(), CredentialEnvRule::Deny)];
        let mut request = plain_request();
        request.set_env_vars.push(("TOKEN".into(), "leak".into()));

        let effective = EffectiveConfig::merge(&session, &request);

        assert!(!effective.env_set.contains_key("TOKEN"));
        assert!(effective.env_unset.contains(&"TOKEN".to_string()));
    }

    #[test]
    fn credential_mask_env_replaces_a_command_unset() {
        let mut session = SessionSandboxConfig::default();
        session.credentials.env_vars = vec![(
            "TOKEN".into(),
            CredentialEnvRule::Mask {
                value: "redacted".into(),
            },
        )];
        let mut request = plain_request();
        request.unset_env_vars.push("TOKEN".into());

        let effective = EffectiveConfig::merge(&session, &request);

        assert_eq!(
            effective.env_set.get("TOKEN").map(String::as_str),
            Some("redacted")
        );
        assert!(!effective.env_unset.contains(&"TOKEN".to_string()));
    }

    #[test]
    fn credential_files_split_into_deny_and_mask() {
        let mut session = SessionSandboxConfig::default();
        session.credentials.files = vec![
            (PathBuf::from("/home/u/.netrc"), CredentialFileRule::Deny),
            (
                PathBuf::from("/home/u/.aws/credentials"),
                CredentialFileRule::Mask {
                    fake: PathBuf::from("/tmp/fake-aws"),
                },
            ),
        ];

        let effective = EffectiveConfig::merge(&session, &plain_request());

        assert!(effective
            .read_rules
            .deny_only
            .contains(&PathBuf::from("/home/u/.netrc")));
        assert_eq!(
            effective.masked_files,
            vec![MaskedFileBind {
                real: PathBuf::from("/home/u/.aws/credentials"),
                fake: PathBuf::from("/tmp/fake-aws"),
            }]
        );
    }

    #[test]
    fn glob_write_roots_are_separated_from_concrete_ones() {
        let session = SessionSandboxConfig {
            filesystem: FilesystemConfig {
                allow_write: vec![PathBuf::from("/work"), PathBuf::from("/work/*/build")],
                ..Default::default()
            },
            ..Default::default()
        };

        let effective = EffectiveConfig::merge(&session, &plain_request());
        let (kept, globs) = effective.concrete_write_roots();

        assert_eq!(kept, vec![PathBuf::from("/work")]);
        assert_eq!(globs, vec![PathBuf::from("/work/*/build")]);
    }

    #[test]
    fn is_within_matches_self_and_descendants_only() {
        assert!(is_within(Path::new("/a"), Path::new("/a")));
        assert!(is_within(Path::new("/a/b"), Path::new("/a")));
        assert!(!is_within(Path::new("/ab"), Path::new("/a")));
        assert!(!is_within(Path::new("/"), Path::new("/a")));
    }

    #[test]
    fn network_restriction_predicate_ignores_capability_switches() {
        let mut network = NetworkConfig {
            allow_local_binding: true,
            allow_all_unix_sockets: true,
            ..Default::default()
        };
        assert!(
            !network.is_restricted(),
            "capability switches alone are not a restriction"
        );
        network.denied_domains.push("evil.test".into());
        assert!(network.is_restricted());
    }
}
