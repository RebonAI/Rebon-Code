//! macOS backend — seatbelt profiles for `sandbox-exec`. RFC §5.
//!
//! Seatbelt is the mirror image of bubblewrap. bubblewrap builds a
//! filesystem the command can only see part of; seatbelt lets the
//! command see everything and refuses the operations it is not
//! allowed. That difference has one consequence worth stating up
//! front: **the profile starts at `(deny default)`**, so anything not
//! named below is refused, including system services the command
//! never asked for by name. The long allowlists in this module are
//! not padding — each entry is a system service that a plain
//! `deny default` profile breaks.
//!
//! It also has a limit the Linux side does not: seatbelt can refuse a
//! read but cannot substitute file contents. A credential *mask*
//! therefore degrades to a *deny* here, and says so through a
//! [`Warning`] rather than silently doing less than asked — RFC §5.4.

use crate::runtime::config::EffectiveConfig;
use crate::runtime::env::EnvPlan;
use crate::runtime::error::{warning_code, SandboxError, Warning};
use std::fmt::Write as _;
use std::path::Path;

pub const BACKEND: &str = "macos";

/// The system binary that applies a profile.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Mach services a command needs before it behaves like a normal
/// process.
///
/// Every name is exact — no wildcards. A `global-name-prefix` here
/// would be the difference between "this command may ask the font
/// server for a font" and "this command may talk to anything whose
/// name starts the same way", and the second is not a sandbox.
const BASE_MACH_SERVICES: &[&str] = &[
    "com.apple.audio.SystemSoundServer-OSX",
    "com.apple.bsd.dirhelper",
    "com.apple.coreservices.launchservicesd",
    "com.apple.distributed_notifications@1v3",
    "com.apple.fonts",
    "com.apple.logd",
    "com.apple.lsd.mapdb",
    "com.apple.PowerManagement.control",
    "com.apple.system.notification_center",
    "com.apple.system.logger",
    "com.apple.system.opendirectoryd.membership",
    "com.apple.SecurityServer",
    "com.apple.securityd.xpc",
];

/// Extra services `open(1)` needs on macOS 14/15. Without them it
/// fails with `-10822`, which reads as "the app is broken" rather
/// than "the sandbox refused a mach lookup".
const APPLE_EVENT_SERVICES: &[&str] = &[
    "com.apple.coreservices.appleevents",
    "com.apple.coreservices.launchservicesd",
    "com.apple.lsd.openurl",
    "com.apple.quarantine-resolver",
];

/// Go's TLS stack verifies certificates through `trustd`, so a Go
/// program behind the sandbox proxy cannot complete a handshake
/// without it. Gated behind `enable_weaker_network_isolation`
/// because the same service can answer questions about certificates
/// the sandbox would rather the command not learn about.
const WEAK_NETWORK_SERVICE: &str = "com.apple.trustd.agent";

/// `sysctl` names that ordinary runtimes read at startup.
///
/// The list is long because a `deny default` profile turns a missing
/// entry into a crash in someone else's code: Node reads `hw.*` for
/// its thread pool, Python reads `kern.*` for `os.cpu_count`, and Go
/// reads `hw.optional.arm*` to pick instruction paths.
///
/// Six of these are read by libsystem at process start rather than by any
/// runtime in particular — `hw.ephemeral_storage`, `kern.bootargs`,
/// `kern.ngroups`, `kern.osvariant_status`, `kern.secure_kernel`,
/// `security.mac.lockdown_mode_state`. Nothing breaks when they are
/// denied, which is why they went unnoticed while violation reporting was
/// broken; what they produce is four to five denials per process, and a
/// single `sh -lc` is three processes. Reporting that on every command
/// would bury the denials that are about the command. They are allowed
/// rather than filtered because none of them is a capability — every one
/// is a scalar any process on the machine can already read, and silencing
/// a denial the sandbox goes on making is a worse trade than not making
/// it.
const SYSCTL_READ: &[&str] = &[
    "hw.activecpu",
    "hw.busfrequency",
    "hw.byteorder",
    "hw.cachelinesize",
    "hw.cpufrequency",
    "hw.cputype",
    "hw.ephemeral_storage",
    "hw.logicalcpu",
    "hw.logicalcpu_max",
    "hw.machine",
    "hw.memsize",
    "hw.model",
    "hw.ncpu",
    "hw.pagesize",
    "hw.physicalcpu",
    "hw.physicalcpu_max",
    "kern.argmax",
    "kern.bootargs",
    "kern.boottime",
    "kern.hostname",
    "kern.maxfilesperproc",
    "kern.ngroups",
    "kern.osproductversion",
    "kern.osrelease",
    "kern.ostype",
    "kern.osvariant_status",
    "kern.osversion",
    "kern.secure_kernel",
    "kern.usrstack64",
    "kern.version",
    "machdep.cpu.brand_string",
    "machdep.cpu.core_count",
    "machdep.cpu.thread_count",
    "security.mac.lockdown_mode_state",
    "vm.loadavg",
];

/// `sysctl` prefixes, where an exact list is not workable — the
/// `hw.optional.*` family alone has dozens of feature flags and grows
/// with every chip revision.
const SYSCTL_READ_PREFIXES: &[&str] = &["hw.optional.", "hw.perflevel", "net.routetable."];

/// Device nodes a command may `ioctl`. Anything not listed is refused
/// — an unrestricted `file-ioctl` reaches disks and network devices.
const IOCTL_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/dtracehelper",
    "/dev/tty",
];

/// Device nodes a command may *write*, reopened after the blanket
/// `(deny file-write*)`.
///
/// Both discard what is written, so the allowance grants no reach: there is
/// nothing to read back and nothing else to affect. `/dev` itself stays
/// unwritable, so neither node can be replaced with something that is not a
/// sink.
///
/// `/dev/tty` is deliberately **not** here. It is a sink in the same sense,
/// but it writes to the real terminal rather than to the pipes the agent
/// captures, which makes it a way for a sandboxed command to put escape
/// sequences on the user's screen unread by anything.
const WRITABLE_DEVICES: &[&str] = &["/dev/null", "/dev/zero"];

/// IOKit user clients needed by anything that touches the window
/// server indirectly (V8's graphics probe, for one).
const IOKIT_USER_CLIENTS: &[&str] = &[
    "IOSurfaceRootUserClient",
    "RootDomainUserClient",
    "IOSurfaceSendRight",
];

/// The generated profile plus whatever it could not express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatbeltProfile {
    pub profile: String,
    pub warnings: Vec<Warning>,
}

/// Escape a path for a seatbelt string literal.
///
/// Seatbelt's parser is a Scheme reader: a `"` ends the string and a
/// backslash escapes. A path containing either would otherwise let
/// the caller close the string and write their own rules, which is
/// the profile-injection equivalent of a shell injection.
fn seatbelt_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn path_literal(path: &Path) -> String {
    seatbelt_string(&path.to_string_lossy())
}

/// Build the seatbelt profile for one command — RFC §5.1.
///
/// `log_tag` is appended to every deny message so the violation
/// monitor can tell this session's denials from any other sandboxed
/// process on the machine. It is generated per session, not per
/// command, and callers must not reuse one across sessions.
pub fn build_seatbelt_profile(config: &EffectiveConfig, log_tag: &str) -> SeatbeltProfile {
    let mut warnings = Vec::new();
    let mut profile = String::with_capacity(4096);

    writeln!(profile, "(version 1)").expect("writing to a String cannot fail");
    writeln!(
        profile,
        "(deny default (with message {}))",
        seatbelt_string(log_tag)
    )
    .expect("writing to a String cannot fail");

    // --- process ---
    // `process-info*` and `signal` are scoped to `same-sandbox` so a
    // command can manage its own children but cannot inspect or kill
    // anything on the host, including the agent that spawned it.
    writeln!(profile, "(allow process-exec)").expect("writing to a String cannot fail");
    writeln!(profile, "(allow process-fork)").expect("writing to a String cannot fail");
    writeln!(profile, "(allow process-info* (target same-sandbox))")
        .expect("writing to a String cannot fail");
    writeln!(profile, "(allow signal (target same-sandbox))")
        .expect("writing to a String cannot fail");
    writeln!(profile, "(allow mach-priv-task-port)").expect("writing to a String cannot fail");

    // --- mach lookup ---
    let mut services: Vec<String> = BASE_MACH_SERVICES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if config.misc.enable_weaker_network_isolation {
        services.push(WEAK_NETWORK_SERVICE.to_string());
    }
    if config.misc.allow_apple_events {
        writeln!(profile, "(allow appleevent-send)").expect("writing to a String cannot fail");
        services.extend(APPLE_EVENT_SERVICES.iter().map(|s| (*s).to_string()));
    }
    writeln!(profile, "(allow mach-lookup").expect("writing to a String cannot fail");
    for service in &services {
        writeln!(profile, "  (global-name {})", seatbelt_string(service))
            .expect("writing to a String cannot fail");
    }
    for pattern in &config.network.allow_mach_lookup {
        // A trailing `*` is the one wildcard the config may ask for,
        // and it becomes a prefix match rather than a glob — seatbelt
        // has no glob, and silently treating `*` as a literal
        // character would make the rule never match.
        match pattern.strip_suffix('*') {
            Some(prefix) => {
                writeln!(
                    profile,
                    "  (global-name-prefix {})",
                    seatbelt_string(prefix)
                )
                .expect("writing to a String cannot fail");
            }
            None => {
                writeln!(profile, "  (global-name {})", seatbelt_string(pattern))
                    .expect("writing to a String cannot fail");
            }
        }
    }
    writeln!(profile, ")").expect("writing to a String cannot fail");

    // --- sysctl ---
    writeln!(profile, "(allow sysctl-read").expect("writing to a String cannot fail");
    for name in SYSCTL_READ {
        writeln!(profile, "  (sysctl-name {})", seatbelt_string(name))
            .expect("writing to a String cannot fail");
    }
    for prefix in SYSCTL_READ_PREFIXES {
        writeln!(
            profile,
            "  (sysctl-name-prefix {})",
            seatbelt_string(prefix)
        )
        .expect("writing to a String cannot fail");
    }
    writeln!(profile, ")").expect("writing to a String cannot fail");
    // V8 sets this to opt threads out of the timer-coalescing that
    // otherwise makes its scheduler miss deadlines. It is the one
    // sysctl write the profile allows, and it affects only the
    // calling thread.
    writeln!(
        profile,
        "(allow sysctl-write (sysctl-name {}))",
        seatbelt_string("kern.tcsm_enable")
    )
    .expect("writing to a String cannot fail");

    // --- IPC ---
    // Python's `multiprocessing` builds its queues out of POSIX
    // shared memory and semaphores; without these it fails at import.
    writeln!(profile, "(allow ipc-posix-shm)").expect("writing to a String cannot fail");
    writeln!(profile, "(allow ipc-posix-sem)").expect("writing to a String cannot fail");

    // --- IOKit ---
    writeln!(profile, "(allow iokit-open").expect("writing to a String cannot fail");
    for client in IOKIT_USER_CLIENTS {
        writeln!(
            profile,
            "  (iokit-user-client-class {})",
            seatbelt_string(client)
        )
        .expect("writing to a String cannot fail");
    }
    writeln!(profile, ")").expect("writing to a String cannot fail");
    writeln!(profile, "(allow iokit-get-properties)").expect("writing to a String cannot fail");

    // --- device ioctl ---
    writeln!(profile, "(allow file-ioctl").expect("writing to a String cannot fail");
    for device in IOCTL_DEVICES {
        writeln!(profile, "  (literal {})", seatbelt_string(device))
            .expect("writing to a String cannot fail");
    }
    writeln!(profile, ")").expect("writing to a String cannot fail");

    // --- pty ---
    if config.misc.allow_pty {
        writeln!(profile, "(allow pseudo-tty)").expect("writing to a String cannot fail");
        writeln!(
            profile,
            "(allow file-read* file-write* (literal {}) (regex #\"^/dev/ttys[0-9]+$\"))",
            seatbelt_string("/dev/ptmx")
        )
        .expect("writing to a String cannot fail");
    }

    // --- network ---
    write_network_rules(&mut profile, config);

    // --- filesystem ---
    write_filesystem_rules(&mut profile, config, &mut warnings);

    SeatbeltProfile { profile, warnings }
}

fn write_network_rules(profile: &mut String, config: &EffectiveConfig) {
    if !config.network_restricted {
        writeln!(profile, "(allow network*)").expect("writing to a String cannot fail");
        return;
    }

    if config.network.allow_local_binding {
        // Bind and accept anywhere local (a dev server picks its own
        // port), but outbound stays pinned to localhost so the
        // permission to listen does not become a permission to call
        // out.
        writeln!(profile, "(allow network-bind (local ip))")
            .expect("writing to a String cannot fail");
        writeln!(profile, "(allow network-inbound (local ip))")
            .expect("writing to a String cannot fail");
        writeln!(
            profile,
            "(allow network-outbound (remote ip {}))",
            seatbelt_string("localhost:*")
        )
        .expect("writing to a String cannot fail");
    }
    if config.network.allow_all_unix_sockets {
        writeln!(profile, "(allow network-bind (local unix-socket))")
            .expect("writing to a String cannot fail");
        writeln!(profile, "(allow network-outbound (remote unix-socket))")
            .expect("writing to a String cannot fail");
    } else {
        for socket in &config.network.allow_unix_sockets {
            writeln!(
                profile,
                "(allow network-bind network-outbound (local unix-socket (subpath {})))",
                path_literal(socket)
            )
            .expect("writing to a String cannot fail");
        }
    }
    for port in [
        config.runtime.http_proxy_port,
        config.runtime.socks_proxy_port,
    ]
    .into_iter()
    .flatten()
    {
        writeln!(
            profile,
            "(allow network-outbound (remote ip {}))",
            seatbelt_string(&format!("localhost:{port}"))
        )
        .expect("writing to a String cannot fail");
    }
}

fn write_filesystem_rules(
    profile: &mut String,
    config: &EffectiveConfig,
    warnings: &mut Vec<Warning>,
) {
    // Reads: open by default, then closed subtrees, then the
    // exceptions punched back into them. Seatbelt takes the *last*
    // matching rule, so the order here is the same contract the Linux
    // mount order carries.
    writeln!(profile, "(allow file-read*)").expect("writing to a String cannot fail");

    if !config.allow_git_config {
        // The global git config routinely names a credential helper,
        // and reading it is enough to learn where the credentials
        // live even without reading them.
        writeln!(
            profile,
            "(deny file-read* (regex #\"^/Users/[^/]+/\\.gitconfig$\"))"
        )
        .expect("writing to a String cannot fail");
    }

    for path in &config.read_rules.deny_only {
        writeln!(
            profile,
            "(deny file-read* (subpath {}))",
            path_literal(path)
        )
        .expect("writing to a String cannot fail");
    }
    // A mask is a deny here, and the caller is told so. Degrading
    // quietly would leave the caller believing the command sees fake
    // credentials when it actually sees a permission error — two very
    // different things to debug.
    for bind in &config.masked_files {
        // Two different things can have happened, and one sentence covering
        // both would be half wrong either way: either the fake is reachable
        // through an environment variable the tool honours, or the command
        // simply gets a permission error. The caller debugs those in two
        // completely different places.
        let detail = match crate::runtime::env::mask_redirect_variable(bind) {
            Some(variable) => format!(
                "seatbelt cannot substitute file contents; {} is denied, and {variable} points \
                 at the fake file so a tool that honours it reads that instead",
                bind.real.display()
            ),
            None => format!(
                "seatbelt cannot substitute file contents; {} is denied instead of masked",
                bind.real.display()
            ),
        };
        warnings.push(Warning::new(
            BACKEND,
            warning_code::MASK_DOWNGRADED_TO_DENY,
            detail,
        ));
        writeln!(
            profile,
            "(deny file-read* (literal {}))",
            path_literal(&bind.real)
        )
        .expect("writing to a String cannot fail");
    }
    for path in &config.read_rules.allow_within_deny {
        writeln!(
            profile,
            "(allow file-read* (subpath {}))",
            path_literal(path)
        )
        .expect("writing to a String cannot fail");
    }

    // Writes: closed by default, then the granted roots, then the
    // holes carved back out of them.
    writeln!(profile, "(deny file-write*)").expect("writing to a String cannot fail");

    // The null sinks come straight back, because `deny default` takes them
    // with everything else and a shell redirect is not an optional feature:
    // `>/dev/null` and `2>/dev/null` appear in a large share of the commands
    // an agent runs, and under the deny they fail *before* the command runs,
    // reporting `Operation not permitted` on a path the user never
    // configured and cannot find in their own settings.
    //
    // `file-write-data` alone is not enough: a `>` redirect opens with
    // `O_CREAT`, which seatbelt checks as `file-write-create` even when the
    // node already exists.
    for device in WRITABLE_DEVICES {
        writeln!(
            profile,
            "(allow file-write-data file-write-create (literal {}))",
            seatbelt_string(device)
        )
        .expect("writing to a String cannot fail");
    }
    let (roots, globs) = config.concrete_write_roots();
    for glob in globs {
        // Unlike bubblewrap, seatbelt *can* express a pattern — but
        // translating a shell glob into its regex dialect is a
        // silent-semantics change (`*` crossing `/`, for one), so the
        // rule is refused on both platforms and the caller sees the
        // same warning either way.
        warnings.push(Warning::new(
            BACKEND,
            warning_code::GLOB_WRITE_PATTERN,
            format!(
                "skipping glob write pattern {} — write roots must be concrete paths",
                glob.display()
            ),
        ));
    }
    for root in &roots {
        writeln!(
            profile,
            "(allow file-write* (subpath {}))",
            path_literal(root)
        )
        .expect("writing to a String cannot fail");
    }
    for path in &config.write_rules.deny_within_allow {
        writeln!(
            profile,
            "(deny file-write* (subpath {}))",
            path_literal(path)
        )
        .expect("writing to a String cannot fail");
    }

    // A read denial inside a write root would otherwise be undone by
    // nothing at all — seatbelt keeps read and write rules apart — but
    // the *write* grant does let the command replace the denied file
    // and read what it wrote. Re-denying reads after the write grants
    // keeps the denial the last word.
    for path in &config.read_rules.deny_only {
        if roots
            .iter()
            .any(|root| crate::runtime::config::is_within(path, root))
        {
            writeln!(
                profile,
                "(deny file-read* (subpath {}))",
                path_literal(path)
            )
            .expect("writing to a String cannot fail");
            writeln!(
                profile,
                "(deny file-write* (subpath {}))",
                path_literal(path)
            )
            .expect("writing to a String cannot fail");
        }
    }
}

/// Build the `sandbox-exec` argv — RFC §5.2.
///
/// The environment is *not* folded into an `env(1)` prefix the way
/// the RFC describes. A Rust spawner sets the child environment
/// directly, and routing it through `env` would add a process, put
/// every variable's value into the process list where any user on the
/// machine can read it, and require another layer of quoting.
pub fn build_seatbelt_argv(
    config: &EffectiveConfig,
    log_tag: &str,
    _env: &EnvPlan,
) -> Result<(String, Vec<String>, Vec<Warning>), SandboxError> {
    if config.bin_shell.program.is_empty() {
        return Err(SandboxError::ShellUnavailable {
            detail: "no shell was configured for the sandboxed command".into(),
        });
    }
    let SeatbeltProfile { profile, warnings } = build_seatbelt_profile(config, log_tag);

    let mut args = vec!["-p".to_string(), profile];
    args.extend(config.bin_shell.argv(&config.command));

    Ok((SANDBOX_EXEC.to_string(), args, warnings))
}

/// The per-session tag that ties a deny message back to this
/// process.
///
/// Not random in the cryptographic sense and does not need to be —
/// it only has to be unlikely to collide with another sandboxed
/// process's tag on the same machine, so that `log stream` does not
/// mix two sessions' violations together.
pub fn session_log_tag(session_id: &str, started_at_nanos: u128) -> String {
    format!("rebon-sbx-{session_id}-{started_at_nanos:x}")
}

/// Violation reporting — RFC §5.3.
///
/// The monitor process itself (`log stream --predicate …`) is the
/// caller's to own; what lives here is the part that decides what a
/// line *means*, because that is the part worth pinning with tests.
pub mod violations {
    /// Noise sources whose denials say nothing about the command.
    ///
    /// These three daemons get denied constantly under any
    /// `deny default` profile — they poll for services the sandbox
    /// has no reason to allow — and surfacing them would bury the
    /// denials that are actually about the user's command.
    pub const NOISE_PROCESSES: &[&str] = &["mDNSResponder", "diagnosticd", "analyticsd"];

    /// A parsed deny message.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Violation {
        /// The operation seatbelt refused, e.g. `file-read-data`.
        pub operation: String,
        /// The whole message, kept verbatim for the audit trail. Spans two
        /// lines: the denial, then the session tag.
        pub raw: String,
    }

    /// The `log stream` predicate for one session's tag.
    pub fn predicate(log_tag: &str) -> String {
        format!("eventMessage ENDSWITH \"{log_tag}\"")
    }

    /// Parse one log event's message into a violation.
    ///
    /// The argument is a whole `eventMessage`, which for a seatbelt denial
    /// is two lines — the denial and then the tag on its own line, because
    /// that is where `(with message ...)` puts it. Both halves have to be in
    /// the same string for this to match anything, which is what
    /// [`crate::runtime::macos_monitor::event_message`] is for.
    ///
    /// Returns `None` for messages that are not denials, do not carry
    /// this session's tag, or come from a known-noisy daemon. The tag
    /// check is what makes two concurrent sessions on one machine
    /// safe — without it each would report the other's violations.
    pub fn parse_line(line: &str, log_tag: &str) -> Option<Violation> {
        if !line.contains(log_tag) {
            return None;
        }
        if NOISE_PROCESSES.iter().any(|noise| line.contains(noise)) {
            return None;
        }
        let marker = line.find("Sandbox:")?;
        let rest = line[marker + "Sandbox:".len()..].trim_start();
        // The shape is `Sandbox: <proc>(<pid>) deny(1) <operation> <path>`.
        let deny = rest.find("deny")?;
        let after_deny = rest[deny..].split_whitespace().nth(1)?;
        Some(Violation {
            operation: after_deny.to_string(),
            raw: line.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::config::{
        BinShell, CommandRequest, CredentialFileRule, EffectiveConfig, ReadRules,
        SessionSandboxConfig, WriteRules,
    };
    use crate::runtime::env::build_env_plan;
    use std::path::PathBuf;

    fn effective(
        mutate: impl FnOnce(&mut SessionSandboxConfig),
        request: CommandRequest,
    ) -> EffectiveConfig {
        let mut session = SessionSandboxConfig::default();
        mutate(&mut session);
        EffectiveConfig::merge(&session, &request)
    }

    fn request() -> CommandRequest {
        CommandRequest::new("echo hi", BinShell::posix())
    }

    fn profile_of(config: &EffectiveConfig) -> String {
        build_seatbelt_profile(config, "tag-1").profile
    }

    #[test]
    fn profile_opens_with_version_and_a_tagged_default_deny() {
        let profile = profile_of(&effective(|_| {}, request()));
        let mut lines = profile.lines();
        assert_eq!(lines.next(), Some("(version 1)"));
        assert_eq!(
            lines.next(),
            Some(r#"(deny default (with message "tag-1"))"#)
        );
    }

    #[test]
    fn process_inspection_and_signals_are_scoped_to_the_sandbox() {
        let profile = profile_of(&effective(|_| {}, request()));
        assert!(profile.contains("(allow process-info* (target same-sandbox))"));
        assert!(profile.contains("(allow signal (target same-sandbox))"));
        assert!(
            !profile.contains("(allow signal)"),
            "an unscoped signal rule would let the command kill the agent"
        );
    }

    #[test]
    fn base_mach_services_are_exact_names_only() {
        let profile = profile_of(&effective(|_| {}, request()));
        assert!(profile.contains(r#"(global-name "com.apple.logd")"#));
        assert!(
            !profile.contains(r#"(global-name-prefix "com.apple.")"#),
            "a broad prefix would defeat the allowlist"
        );
    }

    #[test]
    fn trustd_is_only_present_under_the_weaker_network_switch() {
        let strict = profile_of(&effective(|_| {}, request()));
        assert!(!strict.contains(WEAK_NETWORK_SERVICE));

        let weak = profile_of(&effective(
            |session| session.misc.enable_weaker_network_isolation = true,
            request(),
        ));
        assert!(weak.contains(WEAK_NETWORK_SERVICE));
    }

    #[test]
    fn apple_events_bring_both_the_operation_and_its_services() {
        let profile = profile_of(&effective(
            |session| session.misc.allow_apple_events = true,
            request(),
        ));
        assert!(profile.contains("(allow appleevent-send)"));
        assert!(profile.contains("com.apple.coreservices.appleevents"));
        assert!(profile.contains("com.apple.quarantine-resolver"));
    }

    #[test]
    fn a_trailing_star_in_a_mach_rule_becomes_a_prefix_match() {
        let profile = profile_of(&effective(
            |session| session.network.allow_mach_lookup = vec!["com.example.svc.*".into()],
            request(),
        ));
        assert!(profile.contains(r#"(global-name-prefix "com.example.svc.")"#));
    }

    #[test]
    fn a_mach_rule_without_a_star_stays_an_exact_name() {
        let profile = profile_of(&effective(
            |session| session.network.allow_mach_lookup = vec!["com.example.svc".into()],
            request(),
        ));
        assert!(profile.contains(r#"(global-name "com.example.svc")"#));
        assert!(!profile.contains("global-name-prefix \"com.example.svc\""));
    }

    #[test]
    fn unrestricted_network_allows_everything_in_one_rule() {
        let profile = profile_of(&effective(|_| {}, request()));
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn restricted_network_never_emits_the_blanket_allow() {
        let profile = profile_of(&effective(|_| {}, request().with_network_restriction(true)));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn local_binding_grants_inbound_but_pins_outbound_to_localhost() {
        let profile = profile_of(&effective(
            |session| session.network.allow_local_binding = true,
            request().with_network_restriction(true),
        ));
        assert!(profile.contains("(allow network-bind (local ip))"));
        assert!(profile.contains("(allow network-inbound (local ip))"));
        assert!(profile.contains(r#"(allow network-outbound (remote ip "localhost:*"))"#));
    }

    #[test]
    fn proxy_ports_are_reachable_under_restriction() {
        let profile = profile_of(&effective(
            |session| {
                session.runtime.http_proxy_port = Some(3128);
                session.runtime.socks_proxy_port = Some(1080);
            },
            request().with_network_restriction(true),
        ));
        assert!(profile.contains(r#"(remote ip "localhost:3128")"#));
        assert!(profile.contains(r#"(remote ip "localhost:1080")"#));
    }

    #[test]
    fn unix_socket_allowlist_uses_subpath_per_entry() {
        let profile = profile_of(&effective(
            |session| session.network.allow_unix_sockets = vec![PathBuf::from("/run/docker.sock")],
            request().with_network_restriction(true),
        ));
        assert!(profile.contains(r#"(local unix-socket (subpath "/run/docker.sock"))"#));
    }

    #[test]
    fn writes_are_denied_before_any_root_is_granted() {
        let profile = profile_of(&effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from("/work")],
            request(),
        ));
        let deny = profile.find("(deny file-write*)").unwrap();
        let allow = profile
            .find(r#"(allow file-write* (subpath "/work"))"#)
            .unwrap();
        assert!(
            deny < allow,
            "seatbelt takes the last match; deny must come first"
        );
    }

    #[test]
    fn a_write_denial_lands_after_the_root_it_carves_out_of() {
        let profile = profile_of(&effective(
            |session| {
                session.filesystem.allow_write = vec![PathBuf::from("/work")];
                session.filesystem.deny_write = vec![PathBuf::from("/work/vendor")];
            },
            request(),
        ));
        let allow = profile
            .find(r#"(allow file-write* (subpath "/work"))"#)
            .unwrap();
        let deny = profile
            .find(r#"(deny file-write* (subpath "/work/vendor"))"#)
            .unwrap();
        assert!(deny > allow);
    }

    #[test]
    fn read_denial_inside_a_write_root_is_reasserted_after_the_grant() {
        let profile = profile_of(&effective(
            |session| {
                session.filesystem.allow_write = vec![PathBuf::from("/work")];
                session.filesystem.deny_read = vec![PathBuf::from("/work/.git")];
            },
            request(),
        ));
        let grant = profile
            .find(r#"(allow file-write* (subpath "/work"))"#)
            .unwrap();
        let last_deny = profile
            .rfind(r#"(deny file-read* (subpath "/work/.git"))"#)
            .unwrap();
        assert!(
            last_deny > grant,
            "a write grant lets the command rewrite and re-read the denied path"
        );
        assert!(profile.contains(r#"(deny file-write* (subpath "/work/.git"))"#));
    }

    #[test]
    fn read_exception_follows_its_denial() {
        let profile = profile_of(&effective(
            |session| {
                session.filesystem.deny_read = vec![PathBuf::from("/secret")];
                session.filesystem.allow_read = vec![PathBuf::from("/secret/public")];
            },
            request(),
        ));
        let deny = profile
            .find(r#"(deny file-read* (subpath "/secret"))"#)
            .unwrap();
        let allow = profile
            .find(r#"(allow file-read* (subpath "/secret/public"))"#)
            .unwrap();
        assert!(allow > deny);
    }

    #[test]
    fn credential_mask_degrades_to_deny_and_says_so() {
        let config = effective(
            |session| {
                session.credentials.files = vec![(
                    PathBuf::from("/home/u/.aws/credentials"),
                    CredentialFileRule::Mask {
                        fake: PathBuf::from("/tmp/fake"),
                    },
                )];
            },
            request(),
        );
        let built = build_seatbelt_profile(&config, "tag-1");

        assert!(built
            .profile
            .contains(r#"(deny file-read* (literal "/home/u/.aws/credentials"))"#));
        assert_eq!(built.warnings.len(), 1);
        assert_eq!(
            built.warnings[0].code,
            warning_code::MASK_DOWNGRADED_TO_DENY
        );
    }

    #[test]
    fn glob_write_root_is_refused_on_macos_too() {
        let config = effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from("/work/*/build")],
            request(),
        );
        let built = build_seatbelt_profile(&config, "tag-1");

        assert_eq!(built.warnings[0].code, warning_code::GLOB_WRITE_PATTERN);
        assert!(!built.profile.contains("/work/*/build"));
    }

    #[test]
    fn git_config_is_denied_unless_allowed() {
        let denied = profile_of(&effective(|_| {}, request()));
        assert!(denied.contains(".gitconfig"));

        let allowed = profile_of(&effective(
            |session| session.filesystem.allow_git_config = true,
            request(),
        ));
        assert!(!allowed.contains(".gitconfig"));
    }

    #[test]
    fn pty_rules_appear_only_when_asked_for() {
        assert!(!profile_of(&effective(|_| {}, request())).contains("pseudo-tty"));
        let with_pty = profile_of(&effective(
            |session| session.misc.allow_pty = true,
            request(),
        ));
        assert!(with_pty.contains("(allow pseudo-tty)"));
        assert!(with_pty.contains("/dev/ptmx"));
    }

    #[test]
    fn a_path_with_a_quote_cannot_close_the_profile_string() {
        let config = effective(
            |session| {
                session.filesystem.deny_read = vec![PathBuf::from(
                    "/tmp/a\") (allow file-read*) (deny nothing \"",
                )]
            },
            request(),
        );
        let profile = profile_of(&config);

        assert!(
            profile.contains(r#"\") (allow file-read*)"#),
            "the injected quote must be escaped, not closing: {profile}"
        );
        assert!(
            !profile.contains("\n(allow file-read*) (deny nothing"),
            "an injected rule must never reach column zero"
        );
    }

    #[test]
    fn ioctl_is_limited_to_the_named_devices() {
        let profile = profile_of(&effective(|_| {}, request()));
        // Anchored on the `file-ioctl` block rather than on a bare
        // `/dev/null` literal: the write rules name that node too, so the
        // looser assertion passed while `file-ioctl` said nothing at all.
        let ioctl = profile.find("(allow file-ioctl\n").expect("an ioctl block");
        let end = profile[ioctl..]
            .find("\n)\n")
            .expect("a closed ioctl block")
            + ioctl;
        assert!(profile[ioctl..end].contains(r#"(literal "/dev/null")"#));
        assert!(
            !profile.contains("(allow file-ioctl)\n"),
            "an unrestricted file-ioctl reaches disks and network devices"
        );
    }

    #[test]
    fn the_startup_sysctls_are_allowed_rather_than_denied_every_time() {
        // libsystem reads these in every process, so a single `sh -lc` —
        // three processes — denied them a dozen times over. Nothing broke,
        // which is why they survived until violation reporting started
        // working and the warnings buried everything else.
        let profile = profile_of(&effective(|_| {}, request()));
        for name in [
            "hw.ephemeral_storage",
            "kern.bootargs",
            "kern.ngroups",
            "kern.osvariant_status",
            "kern.secure_kernel",
            "security.mac.lockdown_mode_state",
        ] {
            assert!(
                profile.contains(&format!(r#"(sysctl-name "{name}")"#)),
                "{name} is read at process start and would be denied on every command"
            );
        }
    }

    #[test]
    fn the_null_sinks_stay_writable_under_the_blanket_deny() {
        // `>/dev/null` is in a large share of the commands an agent runs.
        // Under `(deny file-write*)` alone the redirect fails before the
        // command starts, and the error names a path the user never put in
        // their settings.
        let profile = profile_of(&effective(|_| {}, request()));

        let deny = profile.find("(deny file-write*)").expect("a blanket deny");
        let null = profile
            .find(r#"(allow file-write-data file-write-create (literal "/dev/null"))"#)
            .expect("/dev/null must be writable");

        // Later rules win in SBPL, so the order is the whole point.
        assert!(deny < null, "the blanket deny would override the sink");
        assert!(
            profile.contains(r#"(allow file-write-data file-write-create (literal "/dev/zero"))"#)
        );
    }

    #[test]
    fn the_terminal_is_not_a_writable_sink() {
        // `/dev/tty` discards nothing — it reaches the user's real terminal,
        // past the pipes the agent reads, which is a place a sandboxed
        // command should not be able to put escape sequences.
        let profile = profile_of(&effective(|_| {}, request()));
        assert!(
            !profile.contains(r#"(allow file-write-data file-write-create (literal "/dev/tty"))"#)
        );
    }

    #[test]
    fn a_denial_is_parsed_when_the_tag_is_on_its_own_line() {
        // The shape seatbelt actually produces: `(with message ...)` is
        // appended after a newline, so the tag never shares a line with the
        // denial it belongs to.
        let message = "Sandbox: sh(30707) deny(1) file-write-create /etc/x\ntag-1";
        let parsed = violations::parse_line(message, "tag-1").unwrap();
        assert_eq!(parsed.operation, "file-write-create");
        assert_eq!(parsed.raw, message);
    }

    #[test]
    fn argv_places_the_profile_before_the_shell() {
        let config = effective(|_| {}, request());
        let env = build_env_plan(&config);

        let (program, args, _) = build_seatbelt_argv(&config, "tag-1", &env).unwrap();

        assert_eq!(program, SANDBOX_EXEC);
        assert_eq!(args[0], "-p");
        assert!(args[1].starts_with("(version 1)"));
        assert_eq!(&args[2..], &["sh", "-lc", "echo hi"]);
    }

    #[test]
    fn a_missing_shell_is_an_error() {
        let config = effective(
            |_| {},
            CommandRequest::new("echo hi", BinShell::new("", Vec::<String>::new())),
        );
        let env = build_env_plan(&config);

        assert!(matches!(
            build_seatbelt_argv(&config, "tag-1", &env),
            Err(SandboxError::ShellUnavailable { .. })
        ));
    }

    #[test]
    fn encoded_command_shells_pass_through_unchanged() {
        // The PowerShell RFC §7.3 shape: the whole `pwsh … -EncodedCommand`
        // prefix is the shell, and the base64 blob is the command.
        let config = effective(
            |_| {},
            CommandRequest::new(
                "ZQBjAGgAbwA=",
                BinShell::new("pwsh", ["-NoProfile", "-NonInteractive", "-EncodedCommand"]),
            ),
        );
        let env = build_env_plan(&config);

        let (_, args, _) = build_seatbelt_argv(&config, "tag-1", &env).unwrap();

        assert_eq!(
            &args[2..],
            &[
                "pwsh",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                "ZQBjAGgAbwA="
            ]
        );
    }

    #[test]
    fn write_rules_from_the_command_narrow_the_profile() {
        let config = effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from("/work")],
            request().with_write_rules(WriteRules {
                allow_only: vec![PathBuf::from("/work/sub")],
                deny_within_allow: Vec::new(),
            }),
        );
        let profile = profile_of(&config);

        assert!(profile.contains(r#"(allow file-write* (subpath "/work/sub"))"#));
        assert!(!profile.contains(r#"(allow file-write* (subpath "/work"))"#));
    }

    #[test]
    fn per_command_read_denials_reach_the_profile() {
        let config = effective(
            |_| {},
            request().with_read_rules(ReadRules {
                deny_only: vec![PathBuf::from("/tmp/scratch")],
                allow_within_deny: Vec::new(),
            }),
        );
        assert!(profile_of(&config).contains(r#"(deny file-read* (subpath "/tmp/scratch"))"#));
    }

    #[test]
    fn session_log_tags_differ_per_session() {
        assert_ne!(session_log_tag("a", 1), session_log_tag("b", 1));
        assert_ne!(session_log_tag("a", 1), session_log_tag("a", 2));
    }

    #[test]
    fn violation_predicate_matches_the_tag_suffix() {
        assert_eq!(
            violations::predicate("tag-1"),
            r#"eventMessage ENDSWITH "tag-1""#
        );
    }

    #[test]
    fn violation_parsing_extracts_the_refused_operation() {
        let line = "2026-08-25 10:00:00 kernel: Sandbox: node(123) deny(1) file-read-data /etc/shadow tag-1";
        let parsed = violations::parse_line(line, "tag-1").unwrap();
        assert_eq!(parsed.operation, "file-read-data");
        assert_eq!(parsed.raw, line);
    }

    #[test]
    fn violations_from_another_session_are_ignored() {
        let line = "Sandbox: node(1) deny(1) file-read-data /x other-tag";
        assert!(violations::parse_line(line, "tag-1").is_none());
    }

    #[test]
    fn noisy_daemons_are_filtered_out() {
        for noise in violations::NOISE_PROCESSES {
            let line = format!("Sandbox: {noise}(1) deny(1) mach-lookup com.apple.x tag-1");
            assert!(
                violations::parse_line(&line, "tag-1").is_none(),
                "{noise} should be filtered"
            );
        }
    }

    #[test]
    fn a_tagged_line_that_is_not_a_denial_is_ignored() {
        let line = "some unrelated log line tag-1";
        assert!(violations::parse_line(line, "tag-1").is_none());
    }
}
