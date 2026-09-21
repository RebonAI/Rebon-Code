//! Linux backend — bubblewrap mount namespaces. RFC §4.
//!
//! Everything bubblewrap enforces is expressed as bind mounts, and
//! bind mounts have one property that shapes this whole module:
//! **order decides**. A later mount over an overlapping path wins.
//! That makes the plan a sequence, not a set, and it is why the
//! interaction fixes in [`build_mount_plan`] exist at all — a write
//! root bound after a read denial re-exposes the very subtree the
//! denial closed, and nothing about the two rules individually says
//! so.
//!
//! The second shaping property is that a mount needs a concrete
//! existing target. There is no bubblewrap equivalent of "deny writes
//! to `/work/*/build`", so a glob write rule is refused rather than
//! approximated — RFC §4.4. Approximating it either way would be
//! wrong in a direction the caller cannot see.

use crate::runtime::config::{is_glob, is_within, EffectiveConfig};
use crate::runtime::env::EnvPlan;
use crate::runtime::error::{warning_code, SandboxError, Warning};
use crate::runtime::fs_probe::FsProbe;
use std::path::{Path, PathBuf};

pub const BACKEND: &str = "linux";

/// One bubblewrap mount operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountOp {
    /// `--bind SRC DEST` — writable.
    Bind { src: PathBuf, dest: PathBuf },
    /// `--ro-bind SRC DEST` — readable, not writable.
    RoBind { src: PathBuf, dest: PathBuf },
    /// `--tmpfs DEST` — an empty writable overlay that hides
    /// whatever was underneath.
    Tmpfs { dest: PathBuf },
    /// `--proc DEST`
    Proc { dest: PathBuf },
    /// `--dev DEST`
    Dev { dest: PathBuf },
}

impl MountOp {
    fn push_args(&self, out: &mut Vec<String>) {
        match self {
            MountOp::Bind { src, dest } => {
                out.push("--bind".into());
                out.push(src.to_string_lossy().into_owned());
                out.push(dest.to_string_lossy().into_owned());
            }
            MountOp::RoBind { src, dest } => {
                out.push("--ro-bind".into());
                out.push(src.to_string_lossy().into_owned());
                out.push(dest.to_string_lossy().into_owned());
            }
            MountOp::Tmpfs { dest } => {
                out.push("--tmpfs".into());
                out.push(dest.to_string_lossy().into_owned());
            }
            MountOp::Proc { dest } => {
                out.push("--proc".into());
                out.push(dest.to_string_lossy().into_owned());
            }
            MountOp::Dev { dest } => {
                out.push("--dev".into());
                out.push(dest.to_string_lossy().into_owned());
            }
        }
    }

    /// The path this operation lands on.
    pub fn dest(&self) -> &Path {
        match self {
            MountOp::Bind { dest, .. }
            | MountOp::RoBind { dest, .. }
            | MountOp::Tmpfs { dest }
            | MountOp::Proc { dest }
            | MountOp::Dev { dest } => dest,
        }
    }
}

/// The ordered mount operations plus every rule that was dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountPlan {
    pub ops: Vec<MountOp>,
    pub warnings: Vec<Warning>,
}

impl MountPlan {
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(self.ops.len() * 3);
        for op in &self.ops {
            op.push_args(&mut args);
        }
        args
    }
}

/// Kernel filesystems that must not be bind-mounted from the host.
///
/// `/proc` and `/sys` are re-created inside the namespace; binding
/// the host's would hand the sandbox a view of every host process.
/// `/dev` is refused as a *target* too — a bind over a device node
/// either breaks the node or, worse, exposes a writable one.
const KERNEL_FS_ROOTS: &[&str] = &["/proc", "/sys", "/dev"];

fn is_kernel_fs(path: &Path) -> bool {
    KERNEL_FS_ROOTS
        .iter()
        .any(|root| is_within(path, Path::new(root)))
}

/// Build the ordered mount plan for one command — RFC §4.1.
///
/// The order below is the security contract, not a style choice:
///
/// 1. **Read-only root.** Everything is readable and nothing is
///    writable until a later rule says otherwise.
/// 2. **Ancestor pins**, before the denials they protect. A pin
///    placed *after* a denial would re-expose it.
/// 3. **Read denials.**
/// 4. **Read exceptions** inside those denials.
/// 5. **Write roots** — the only rules that grant, so they come
///    after everything that closes.
/// 6. **Write denials** inside those roots.
/// 7. **Credential masks.**
/// 8. **Re-application** of any denial or mask a write root
///    re-exposed in step 5.
/// 9. **Kernel filesystems**, last, so nothing can shadow them.
pub fn build_mount_plan(config: &EffectiveConfig, fs: &dyn FsProbe) -> MountPlan {
    let mut plan = MountPlan::default();

    let (write_roots, glob_roots) = config.concrete_write_roots();
    for glob in &glob_roots {
        plan.warnings.push(Warning::new(
            BACKEND,
            warning_code::GLOB_WRITE_PATTERN,
            format!(
                "skipping glob write pattern {} — a mount needs a concrete target",
                glob.display()
            ),
        ));
    }

    // 1. Read-only root.
    plan.ops.push(MountOp::RoBind {
        src: PathBuf::from("/"),
        dest: PathBuf::from("/"),
    });

    // 2. Ancestor pins for every path a denial or mask lands on.
    let mut pinned: Vec<PathBuf> = Vec::new();
    let pin_targets: Vec<&PathBuf> = config
        .read_rules
        .deny_only
        .iter()
        .chain(config.write_rules.deny_within_allow.iter())
        .chain(config.masked_files.iter().map(|bind| &bind.real))
        .collect();
    for target in pin_targets {
        pin_ancestor(target, &write_roots, fs, &mut plan, &mut pinned);
    }

    // 3. Read denials.
    let mut deny_read_ops: Vec<MountOp> = Vec::new();
    for path in &config.read_rules.deny_only {
        match deny_path_op(path, fs) {
            Ok(Some(op)) => deny_read_ops.push(op),
            Ok(None) => {}
            Err(warning) => plan.warnings.push(warning),
        }
    }
    plan.ops.extend(deny_read_ops.iter().cloned());

    // 4. Read exceptions punched back into those denials.
    for path in &config.read_rules.allow_within_deny {
        if !fs.exists(path) {
            plan.warnings.push(Warning::new(
                BACKEND,
                warning_code::PATH_MISSING,
                format!("read exception {} does not exist", path.display()),
            ));
            continue;
        }
        plan.ops.push(MountOp::RoBind {
            src: path.clone(),
            dest: path.clone(),
        });
    }

    // 5. Write roots.
    let mut bound_write_roots: Vec<PathBuf> = Vec::new();
    for root in &write_roots {
        match writable_bind(root, fs) {
            Ok(op) => {
                bound_write_roots.push(root.clone());
                plan.ops.push(op);
            }
            Err(warning) => plan.warnings.push(warning),
        }
    }

    // 6. Write denials inside those roots.
    let mut deny_write_ops: Vec<MountOp> = Vec::new();
    for path in &config.write_rules.deny_within_allow {
        match deny_path_op(path, fs) {
            Ok(Some(op)) => deny_write_ops.push(op),
            Ok(None) => {}
            Err(warning) => plan.warnings.push(warning),
        }
    }
    plan.ops.extend(deny_write_ops);

    // 7. Credential masks.
    let mut mask_ops: Vec<MountOp> = Vec::new();
    for bind in &config.masked_files {
        if !fs.exists(&bind.fake) {
            plan.warnings.push(Warning::new(
                BACKEND,
                warning_code::PATH_MISSING,
                format!(
                    "credential mask source {} does not exist; {} stays visible",
                    bind.fake.display(),
                    bind.real.display()
                ),
            ));
            continue;
        }
        mask_ops.push(MountOp::RoBind {
            src: bind.fake.clone(),
            dest: bind.real.clone(),
        });
    }
    plan.ops.extend(mask_ops.iter().cloned());

    // 8. Re-apply anything a write root re-exposed.
    //
    // A `--bind /work /work` re-binds the whole real subtree, so a
    // denial or mask under `/work` that was placed in step 3 or 7 is
    // gone again. Neither rule mentions the other; only the ordering
    // makes them interact, so the fix belongs here rather than in
    // either rule's own branch.
    for op in deny_read_ops.iter().chain(mask_ops.iter()) {
        if bound_write_roots
            .iter()
            .any(|root| is_within(op.dest(), root) && op.dest() != root.as_path())
        {
            plan.ops.push(op.clone());
        }
    }

    // 8b. The proxy sockets, if this command is network-restricted.
    //
    // `--unshare-net` gives the sandbox a private loopback, so the host's
    // proxy port is unreachable from inside — the socket file is the only
    // thing that crosses, and it only crosses if it is mounted. Without
    // this the in-sandbox forwarder ([`bridge`]) points `socat` at a path
    // that does not exist inside, and every request hangs until it gives
    // up. Bound read-write because a unix socket is connected by writing
    // to it.
    //
    // After the write roots and re-applications on purpose: a socket under
    // a denied path must still work, since it is the sandbox's only route
    // to the network it was explicitly granted.
    if config.network_restricted {
        for socket in [
            config.runtime.http_proxy_socket.as_ref(),
            config.runtime.socks_proxy_socket.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if fs.exists(socket) {
                plan.ops.push(MountOp::Bind {
                    src: socket.clone(),
                    dest: socket.clone(),
                });
            } else {
                plan.warnings.push(Warning::new(
                    BACKEND,
                    warning_code::PATH_MISSING,
                    format!(
                        "the sandbox proxy socket {} does not exist, so the confined \
                         command has no route to the network it was granted",
                        socket.display()
                    ),
                ));
            }
        }
    }

    // 9. Kernel filesystems, last.
    plan.ops.push(MountOp::Proc {
        dest: PathBuf::from("/proc"),
    });
    plan.ops.push(MountOp::Dev {
        dest: PathBuf::from("/dev"),
    });

    plan
}

/// The mount that closes `path`, or `None` when there is nothing to
/// close because the path does not exist.
///
/// A missing deny target is not an error: the read-only root already
/// means nothing can be created there, so the denial is satisfied by
/// the base mount. Warning about it would make every session with a
/// standard credential list print warnings for the files the user
/// happens not to have.
fn deny_path_op(path: &Path, fs: &dyn FsProbe) -> Result<Option<MountOp>, Warning> {
    if is_kernel_fs(path) {
        return Err(Warning::new(
            BACKEND,
            warning_code::DEV_PATH_REFUSED,
            format!(
                "refusing to place a denial on kernel filesystem path {}",
                path.display()
            ),
        ));
    }
    if !fs.exists(path) {
        return Ok(None);
    }
    if fs.is_dir(path) {
        Ok(Some(MountOp::Tmpfs {
            dest: path.to_path_buf(),
        }))
    } else {
        // `/dev/null` over a file is the file-shaped equivalent of an
        // empty tmpfs: the path still opens, and reads see nothing.
        Ok(Some(MountOp::RoBind {
            src: PathBuf::from("/dev/null"),
            dest: path.to_path_buf(),
        }))
    }
}

/// A write root, after the three checks RFC §4.1.1 requires.
fn writable_bind(root: &Path, fs: &dyn FsProbe) -> Result<MountOp, Warning> {
    if is_glob(root) {
        return Err(Warning::new(
            BACKEND,
            warning_code::GLOB_WRITE_PATTERN,
            format!("skipping glob write pattern {}", root.display()),
        ));
    }
    if is_kernel_fs(root) {
        return Err(Warning::new(
            BACKEND,
            warning_code::DEV_PATH_REFUSED,
            format!(
                "refusing to open kernel filesystem path {} for writing",
                root.display()
            ),
        ));
    }
    if !fs.exists(root) {
        return Err(Warning::new(
            BACKEND,
            warning_code::PATH_MISSING,
            format!("write root {} does not exist", root.display()),
        ));
    }
    // The check that matters: a symlinked write root would open the
    // subtree it points at, which may be entirely outside the
    // configured roots. Comparing against `realpath` is what turns
    // "grant /work" into "grant the directory /work actually is".
    match fs.real_path(root) {
        Some(real) if real == root => Ok(MountOp::Bind {
            src: root.to_path_buf(),
            dest: root.to_path_buf(),
        }),
        Some(real) => Err(Warning::new(
            BACKEND,
            warning_code::SYMLINK_ESCAPE,
            format!(
                "write root {} resolves to {} — refusing to bind a redirected path",
                root.display(),
                real.display()
            ),
        )),
        None => Err(Warning::new(
            BACKEND,
            warning_code::SYMLINK_ESCAPE,
            format!("write root {} cannot be resolved", root.display()),
        )),
    }
}

/// Pin the parent of a denied path so it cannot be swapped out.
///
/// Without this, a denial on `/secret/key` is defeated by replacing
/// `/secret` with a symlink or a fresh directory: the mount lands on
/// a path that no longer names the thing being protected. Pinning the
/// parent read-only makes the parent itself immutable inside the
/// sandbox.
///
/// Two cases skip the pin, both deliberately:
///
/// * the parent is inside a write root — the write bind already pins
///   that subtree, and a read-only pin here would silently take the
///   granted write away;
/// * any component of the parent is missing or is itself a symlink —
///   the path is already not what it claims to be, so pinning it
///   would pin the wrong thing. That case warns.
///
/// A denial whose target does not exist is not pinned at all, and
/// does not warn: there is nothing there to protect, the read-only
/// root already prevents creating it, and warning would mean every
/// session printed a line for each credential file the user happens
/// not to have.
fn pin_ancestor(
    target: &Path,
    write_roots: &[PathBuf],
    fs: &dyn FsProbe,
    plan: &mut MountPlan,
    pinned: &mut Vec<PathBuf>,
) {
    if !fs.exists(target) {
        return;
    }
    let Some(parent) = target.parent() else {
        return;
    };
    if parent.as_os_str().is_empty() || parent == Path::new("/") {
        return;
    }
    if pinned.iter().any(|p| p == parent) {
        return;
    }
    if is_kernel_fs(parent) {
        return;
    }
    if write_roots.iter().any(|root| is_within(parent, root)) {
        return;
    }

    let mut walked = PathBuf::from("/");
    for component in parent.components().skip(1) {
        walked.push(component);
        if !fs.exists(&walked) {
            plan.warnings.push(Warning::new(
                BACKEND,
                warning_code::ANCESTOR_UNSTABLE,
                format!(
                    "dropping ancestor pin for {}: {} does not exist",
                    target.display(),
                    walked.display()
                ),
            ));
            return;
        }
        if fs.is_symlink(&walked) {
            plan.warnings.push(Warning::new(
                BACKEND,
                warning_code::ANCESTOR_UNSTABLE,
                format!(
                    "dropping ancestor pin for {}: {} is a symlink",
                    target.display(),
                    walked.display()
                ),
            ));
            return;
        }
    }

    pinned.push(parent.to_path_buf());
    plan.ops.push(MountOp::RoBind {
        src: parent.to_path_buf(),
        dest: parent.to_path_buf(),
    });
}

/// Build the full `bwrap` argv — RFC §4.2.
///
/// `--unshare-all` drops every namespace, network included, and
/// `--share-net` puts the network back when the command was not
/// asked to be network-restricted. Doing it in that order (drop
/// everything, then re-grant one thing) means a future namespace
/// bubblewrap learns about is dropped by default rather than
/// silently shared.
pub fn build_bwrap_argv(
    config: &EffectiveConfig,
    env: &EnvPlan,
    fs: &dyn FsProbe,
) -> Result<(String, Vec<String>, Vec<Warning>), SandboxError> {
    let bwrap =
        config
            .runtime
            .bwrap_path
            .clone()
            .ok_or_else(|| SandboxError::MissingDependency {
                dependency: "bwrap",
                detail: "bubblewrap is required to sandbox commands on Linux \
                     (for example: apt install bubblewrap)"
                    .into(),
            })?;

    let plan = build_mount_plan(config, fs);
    let mut args = plan.to_args();

    args.push("--unshare-all".into());
    if !config.network_restricted {
        args.push("--share-net".into());
    }
    // A fresh session keeps the sandboxed process off the terminal's
    // controlling tty, so it cannot inject keystrokes into the shell
    // that launched it via `TIOCSTI`.
    args.push("--new-session".into());
    args.push("--die-with-parent".into());

    for (key, value) in &env.set {
        args.push("--setenv".into());
        args.push(key.clone());
        args.push(value.clone());
    }
    for key in &env.unset {
        args.push("--unsetenv".into());
        args.push(key.clone());
    }
    if let Some(cwd) = &config.cwd {
        args.push("--chdir".into());
        args.push(cwd.to_string_lossy().into_owned());
    }

    args.push("--".into());
    args.extend(bridge::wrap_argv(config)?);

    Ok((bwrap.to_string_lossy().into_owned(), args, plan.warnings))
}

/// The loopback bridge that gets a network-restricted command to the
/// proxy — RFC §4.3.
///
/// Bubblewrap's `--unshare-net` gives the sandbox a *private*
/// loopback. A proxy listening on the host's `127.0.0.1:3128` is
/// therefore unreachable from inside, which is the whole reason this
/// module exists: the proxy also accepts on a unix socket, the socket
/// file crosses the namespace as an ordinary bind mount, and a
/// `socat` started inside the sandbox re-publishes it on the sandbox's
/// own loopback at the port the proxy environment variables name.
///
/// This is the one place the implementation deviates from the RFC's
/// literal text, which starts `socat` on the host. A host-side
/// listener cannot be reached through `--unshare-net`, so the
/// forwarder has to be on the inside.
pub mod bridge {
    use super::*;

    /// Wrap the command's argv with the in-sandbox forwarders, if any
    /// are needed.
    ///
    /// Returns the argv unchanged whenever there is no restriction or
    /// no proxy socket to forward to — an unrestricted command must
    /// not pay for a shell it does not need, and pointing at a
    /// forwarder for a socket that does not exist would turn "no
    /// network" into "every connection hangs".
    pub fn wrap_argv(config: &EffectiveConfig) -> Result<Vec<String>, SandboxError> {
        let forwarders = forwarder_commands(config)?;
        let inner = config.bin_shell.argv(&config.command);
        if forwarders.is_empty() {
            return Ok(inner);
        }

        let mut script = String::new();
        for forwarder in &forwarders {
            script.push_str(forwarder);
            script.push_str(" &\n");
        }
        // Without the trap the forwarders outlive the command and the
        // sandbox never exits; `--die-with-parent` covers the crash
        // case, this covers the ordinary one.
        script.push_str("trap 'kill 0' EXIT\n");
        script.push_str("exec ");
        script.push_str(&shell_join(&inner));
        script.push('\n');

        Ok(vec!["/bin/sh".to_string(), "-c".to_string(), script])
    }

    fn forwarder_commands(config: &EffectiveConfig) -> Result<Vec<String>, SandboxError> {
        if !config.network_restricted {
            return Ok(Vec::new());
        }
        let runtime = &config.runtime;
        let pairs = [
            (runtime.http_proxy_port, runtime.http_proxy_socket.as_ref()),
            (
                runtime.socks_proxy_port,
                runtime.socks_proxy_socket.as_ref(),
            ),
        ];
        if pairs
            .iter()
            .all(|(port, socket)| port.is_none() || socket.is_none())
        {
            return Ok(Vec::new());
        }

        let socat = runtime
            .socat_path
            .clone()
            .ok_or_else(|| SandboxError::MissingDependency {
                dependency: "socat",
                detail: "socat is required to reach the sandbox proxy on Linux \
                         (for example: apt install socat)"
                    .into(),
            })?;
        let socat = socat.to_string_lossy().into_owned();

        let mut commands = Vec::new();
        for (port, socket) in pairs {
            let (Some(port), Some(socket)) = (port, socket) else {
                continue;
            };
            commands.push(format!(
                "{} TCP-LISTEN:{},fork,reuseaddr,bind=127.0.0.1 UNIX-CONNECT:{}",
                shell_quote(&socat),
                port,
                shell_quote(&socket.to_string_lossy())
            ));
        }
        Ok(commands)
    }

    /// POSIX single-quote escaping.
    ///
    /// The command payload reaching this function is attacker-shaped
    /// by definition — it is whatever the model asked to run — so the
    /// quoting has to be the total kind: wrap in single quotes and
    /// replace each embedded quote with `'\''`. Nothing inside single
    /// quotes is special to `sh`, which is what makes this safe
    /// rather than merely careful.
    pub fn shell_quote(value: &str) -> String {
        let mut quoted = String::with_capacity(value.len() + 2);
        quoted.push('\'');
        for ch in value.chars() {
            if ch == '\'' {
                quoted.push_str("'\\''");
            } else {
                quoted.push(ch);
            }
        }
        quoted.push('\'');
        quoted
    }

    pub fn shell_join(argv: &[String]) -> String {
        argv.iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::config::{
        BinShell, CommandRequest, CredentialFileRule, EffectiveConfig, FilesystemConfig, ReadRules,
        SessionSandboxConfig, WriteRules,
    };
    use crate::runtime::env::build_env_plan;
    use crate::runtime::fs_probe::FakeFs;

    fn effective(
        mutate: impl FnOnce(&mut SessionSandboxConfig),
        request: CommandRequest,
    ) -> EffectiveConfig {
        let mut session = SessionSandboxConfig::default();
        session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
        mutate(&mut session);
        EffectiveConfig::merge(&session, &request)
    }

    fn request() -> CommandRequest {
        CommandRequest::new("echo hi", BinShell::posix())
    }

    fn args_of(plan: &MountPlan) -> String {
        plan.to_args().join(" ")
    }

    #[test]
    fn base_plan_is_a_read_only_root_plus_kernel_filesystems() {
        let config = effective(|_| {}, request());
        let plan = build_mount_plan(&config, &FakeFs::new());

        assert_eq!(
            plan.ops.first(),
            Some(&MountOp::RoBind {
                src: PathBuf::from("/"),
                dest: PathBuf::from("/"),
            })
        );
        assert_eq!(
            plan.ops.last(),
            Some(&MountOp::Dev {
                dest: PathBuf::from("/dev")
            })
        );
        assert!(args_of(&plan).contains("--proc /proc"));
    }

    #[test]
    fn deny_read_directory_becomes_a_tmpfs() {
        let fs = FakeFs::new().dir("/secret").dir("/");
        let config = effective(
            |session| session.filesystem.deny_read = vec![PathBuf::from("/secret")],
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert!(plan.ops.contains(&MountOp::Tmpfs {
            dest: PathBuf::from("/secret")
        }));
    }

    #[test]
    fn deny_read_file_becomes_a_dev_null_bind() {
        let fs = FakeFs::new().file("/home/u/.netrc").dir("/home/u");
        let config = effective(
            |session| session.filesystem.deny_read = vec![PathBuf::from("/home/u/.netrc")],
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert!(plan.ops.contains(&MountOp::RoBind {
            src: PathBuf::from("/dev/null"),
            dest: PathBuf::from("/home/u/.netrc"),
        }));
    }

    #[test]
    fn missing_deny_target_is_skipped_without_a_warning() {
        let config = effective(
            |session| session.filesystem.deny_read = vec![PathBuf::from("/home/u/.netrc")],
            request(),
        );

        let plan = build_mount_plan(&config, &FakeFs::new());

        assert!(
            plan.warnings.is_empty(),
            "a credential file the user does not have is not a problem: {:?}",
            plan.warnings
        );
        assert!(!plan
            .ops
            .iter()
            .any(|op| op.dest() == Path::new("/home/u/.netrc")));
    }

    #[test]
    fn glob_write_root_is_refused_with_a_warning() {
        let config = effective(
            |session| {
                session.filesystem.allow_write =
                    vec![PathBuf::from("/work"), PathBuf::from("/work/*/build")]
            },
            request(),
        );
        let fs = FakeFs::new().dir("/work");

        let plan = build_mount_plan(&config, &fs);

        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.warnings[0].code, warning_code::GLOB_WRITE_PATTERN);
        assert!(plan.ops.contains(&MountOp::Bind {
            src: PathBuf::from("/work"),
            dest: PathBuf::from("/work"),
        }));
    }

    #[test]
    fn symlinked_write_root_is_refused() {
        let fs = FakeFs::new().dir("/real").symlink("/work", "/real");
        let config = effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from("/work")],
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.warnings[0].code, warning_code::SYMLINK_ESCAPE);
        assert!(!plan.ops.iter().any(|op| matches!(op, MountOp::Bind { .. })));
    }

    #[test]
    fn missing_write_root_is_refused() {
        let config = effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from("/work")],
            request(),
        );

        let plan = build_mount_plan(&config, &FakeFs::new());

        assert_eq!(plan.warnings[0].code, warning_code::PATH_MISSING);
    }

    #[test]
    fn kernel_filesystem_write_root_is_refused() {
        let fs = FakeFs::new().dir("/proc");
        let config = effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from("/proc")],
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert_eq!(plan.warnings[0].code, warning_code::DEV_PATH_REFUSED);
    }

    #[test]
    fn write_root_reexposing_a_denial_reapplies_it_afterwards() {
        let fs = FakeFs::new().dir("/work").dir("/work/.git");
        let config = effective(
            |session| {
                session.filesystem = FilesystemConfig {
                    allow_write: vec![PathBuf::from("/work")],
                    deny_read: vec![PathBuf::from("/work/.git")],
                    ..Default::default()
                };
            },
            request(),
        );

        let plan = build_mount_plan(&config, &fs);
        let tmpfs_positions: Vec<usize> = plan
            .ops
            .iter()
            .enumerate()
            .filter(
                |(_, op)| matches!(op, MountOp::Tmpfs { dest } if dest == Path::new("/work/.git")),
            )
            .map(|(index, _)| index)
            .collect();
        let bind_position = plan
            .ops
            .iter()
            .position(|op| matches!(op, MountOp::Bind { dest, .. } if dest == Path::new("/work")))
            .expect("write root bound");

        assert_eq!(
            tmpfs_positions.len(),
            2,
            "the denial must be applied again after the write bind re-exposed it"
        );
        assert!(tmpfs_positions.iter().any(|pos| *pos > bind_position));
    }

    #[test]
    fn a_denial_outside_every_write_root_is_applied_once() {
        let fs = FakeFs::new().dir("/work").dir("/secret");
        let config = effective(
            |session| {
                session.filesystem = FilesystemConfig {
                    allow_write: vec![PathBuf::from("/work")],
                    deny_read: vec![PathBuf::from("/secret")],
                    ..Default::default()
                };
            },
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert_eq!(
            plan.ops
                .iter()
                .filter(|op| op.dest() == Path::new("/secret"))
                .count(),
            1
        );
    }

    #[test]
    fn credential_mask_binds_the_fake_over_the_real_path() {
        let fs = FakeFs::new()
            .dir("/home")
            .dir("/home/u")
            .dir("/home/u/.aws")
            .file("/home/u/.aws/credentials")
            .file("/tmp/fake");
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

        let plan = build_mount_plan(&config, &fs);

        assert!(plan.ops.contains(&MountOp::RoBind {
            src: PathBuf::from("/tmp/fake"),
            dest: PathBuf::from("/home/u/.aws/credentials"),
        }));
    }

    #[test]
    fn missing_mask_source_warns_rather_than_leaving_a_broken_mount() {
        let fs = FakeFs::new()
            .dir("/home")
            .dir("/home/u")
            .dir("/home/u/.aws")
            .file("/home/u/.aws/credentials");
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

        let plan = build_mount_plan(&config, &fs);

        assert_eq!(plan.warnings[0].code, warning_code::PATH_MISSING);
        assert!(!plan
            .ops
            .iter()
            .any(|op| op.dest() == Path::new("/home/u/.aws/credentials")));
    }

    #[test]
    fn ancestor_pin_precedes_the_denial_it_protects() {
        let fs = FakeFs::new().dir("/secret").file("/secret/key");
        let config = effective(
            |session| session.filesystem.deny_read = vec![PathBuf::from("/secret/key")],
            request(),
        );

        let plan = build_mount_plan(&config, &fs);
        let pin = plan
            .ops
            .iter()
            .position(|op| matches!(op, MountOp::RoBind { dest, src } if dest == Path::new("/secret") && src == Path::new("/secret")))
            .expect("ancestor pinned");
        let denial = plan
            .ops
            .iter()
            .position(|op| op.dest() == Path::new("/secret/key"))
            .expect("denial placed");

        assert!(pin < denial, "a pin placed after the denial would undo it");
    }

    #[test]
    fn symlinked_ancestor_drops_the_pin_with_a_warning() {
        let fs = FakeFs::new()
            .dir("/real")
            .symlink("/secret", "/real")
            .file("/secret/key");
        let config = effective(
            |session| session.filesystem.deny_read = vec![PathBuf::from("/secret/key")],
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert!(plan
            .warnings
            .iter()
            .any(|w| w.code == warning_code::ANCESTOR_UNSTABLE));
    }

    #[test]
    fn ancestor_inside_a_write_root_is_not_pinned_read_only() {
        let fs = FakeFs::new()
            .dir("/work")
            .dir("/work/sub")
            .file("/work/sub/f");
        let config = effective(
            |session| {
                session.filesystem = FilesystemConfig {
                    allow_write: vec![PathBuf::from("/work")],
                    deny_read: vec![PathBuf::from("/work/sub/f")],
                    ..Default::default()
                };
            },
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert!(
            !plan.ops.iter().any(|op| matches!(
                op,
                MountOp::RoBind { dest, src } if dest == Path::new("/work/sub") && src == Path::new("/work/sub")
            )),
            "pinning inside a write root would revoke the granted write"
        );
    }

    #[test]
    fn read_exception_is_rebound_after_the_denial() {
        let fs = FakeFs::new()
            .dir("/secret")
            .dir("/secret/public")
            .file("/secret/key");
        let config = effective(
            |session| {
                session.filesystem.deny_read = vec![PathBuf::from("/secret")];
                session.filesystem.allow_read = vec![PathBuf::from("/secret/public")];
            },
            request(),
        );

        let plan = build_mount_plan(&config, &fs);
        let denial = plan
            .ops
            .iter()
            .position(|op| op.dest() == Path::new("/secret"))
            .unwrap();
        let exception = plan
            .ops
            .iter()
            .position(|op| op.dest() == Path::new("/secret/public"))
            .unwrap();

        assert!(exception > denial);
    }

    #[test]
    fn unrestricted_network_shares_the_host_network() {
        let config = effective(|_| {}, request());
        let env = build_env_plan(&config);

        let (program, args, _) = build_bwrap_argv(&config, &env, &FakeFs::new()).unwrap();

        assert_eq!(program, "/usr/bin/bwrap");
        assert!(args.contains(&"--unshare-all".to_string()));
        assert!(args.contains(&"--share-net".to_string()));
    }

    #[test]
    fn restricted_network_does_not_share_the_host_network() {
        let config = effective(|_| {}, request().with_network_restriction(true));
        let env = build_env_plan(&config);

        let (_, args, _) = build_bwrap_argv(&config, &env, &FakeFs::new()).unwrap();

        assert!(args.contains(&"--unshare-all".to_string()));
        assert!(!args.contains(&"--share-net".to_string()));
    }

    #[test]
    fn missing_bwrap_is_an_error_not_a_passthrough() {
        let mut session = SessionSandboxConfig::default();
        session.filesystem.deny_read = vec![PathBuf::from("/secret")];
        let config = EffectiveConfig::merge(&session, &request());
        let env = build_env_plan(&config);

        let error = build_bwrap_argv(&config, &env, &FakeFs::new()).unwrap_err();

        assert!(matches!(
            error,
            SandboxError::MissingDependency {
                dependency: "bwrap",
                ..
            }
        ));
    }

    #[test]
    fn env_plan_reaches_bwrap_as_setenv_and_unsetenv() {
        let mut req = request().with_network_restriction(true);
        req.set_env_vars.push(("FOO".into(), "bar".into()));
        let config = effective(|_| {}, req);
        let env = build_env_plan(&config);

        let (_, args, _) = build_bwrap_argv(&config, &env, &FakeFs::new()).unwrap();
        let joined = args.join(" ");

        assert!(joined.contains("--setenv FOO bar"));
        assert!(joined.contains("--unsetenv no_proxy"));
    }

    #[test]
    fn cwd_becomes_chdir() {
        let config = effective(|_| {}, request().with_cwd("/work"));
        let env = build_env_plan(&config);

        let (_, args, _) = build_bwrap_argv(&config, &env, &FakeFs::new()).unwrap();

        assert!(args.join(" ").contains("--chdir /work"));
    }

    #[test]
    fn command_argv_follows_the_double_dash() {
        let config = effective(|_| {}, request());
        let env = build_env_plan(&config);

        let (_, args, _) = build_bwrap_argv(&config, &env, &FakeFs::new()).unwrap();
        let separator = args.iter().position(|a| a == "--").unwrap();

        assert_eq!(&args[separator + 1..], &["sh", "-lc", "echo hi"]);
    }

    #[test]
    fn the_proxy_socket_is_mounted_into_the_sandbox() {
        // The forwarder inside the sandbox points `socat` at this path. If
        // the mount plan does not carry the socket across the namespace,
        // that path does not exist inside and every request hangs until it
        // gives up — which reads as a broken network rather than a missing
        // mount.
        let fs = FakeFs::new().dir("/run/sbx").file("/run/sbx/http.sock");
        let config = effective(
            |session| {
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            request().with_network_restriction(true),
        );

        let plan = build_mount_plan(&config, &fs);

        assert!(
            plan.ops.iter().any(|op| matches!(
                op,
                MountOp::Bind { src, dest }
                    if src == Path::new("/run/sbx/http.sock")
                        && dest == Path::new("/run/sbx/http.sock")
            )),
            "the proxy socket never reaches the sandbox: {:?}",
            plan.ops
        );
    }

    #[test]
    fn the_proxy_socket_is_mounted_before_the_kernel_filesystems() {
        // Step 9 is last for a reason; a mount added after `--proc` and
        // `--dev` could shadow them.
        let fs = FakeFs::new().dir("/run/sbx").file("/run/sbx/http.sock");
        let config = effective(
            |session| {
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            request().with_network_restriction(true),
        );

        let plan = build_mount_plan(&config, &fs);
        let socket = plan
            .ops
            .iter()
            .position(|op| op.dest() == Path::new("/run/sbx/http.sock"))
            .unwrap();
        let proc = plan
            .ops
            .iter()
            .position(|op| matches!(op, MountOp::Proc { .. }))
            .unwrap();

        assert!(socket < proc, "{:?}", plan.ops);
    }

    #[test]
    fn an_unrestricted_command_does_not_get_the_proxy_socket() {
        // It has the host's network already; mounting the proxy in would be
        // a path the command can reach for no reason.
        let fs = FakeFs::new().dir("/run/sbx").file("/run/sbx/http.sock");
        let config = effective(
            |session| {
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            request(),
        );

        let plan = build_mount_plan(&config, &fs);

        assert!(!plan
            .ops
            .iter()
            .any(|op| op.dest() == Path::new("/run/sbx/http.sock")));
    }

    #[test]
    fn a_missing_proxy_socket_warns_rather_than_mounting_nothing_silently() {
        let config = effective(
            |session| {
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            request().with_network_restriction(true),
        );

        let plan = build_mount_plan(&config, &FakeFs::new());

        let warning = plan
            .warnings
            .iter()
            .find(|warning| warning.detail.contains("http.sock"))
            .expect("a proxy the command cannot reach must be reported");
        assert!(
            warning.detail.contains("no route to the network"),
            "{warning:?}"
        );
    }

    #[test]
    fn no_proxy_socket_means_no_forwarder_shell() {
        let config = effective(|_| {}, request().with_network_restriction(true));

        let argv = bridge::wrap_argv(&config).unwrap();

        assert_eq!(argv, vec!["sh", "-lc", "echo hi"]);
    }

    #[test]
    fn proxy_socket_wraps_the_command_in_a_forwarder_shell() {
        let config = effective(
            |session| {
                session.runtime.socat_path = Some(PathBuf::from("/usr/bin/socat"));
                session.runtime.http_proxy_port = Some(3128);
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            request().with_network_restriction(true),
        );

        let argv = bridge::wrap_argv(&config).unwrap();

        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("TCP-LISTEN:3128,fork,reuseaddr,bind=127.0.0.1"));
        assert!(argv[2].contains("UNIX-CONNECT:'/run/sbx/http.sock'"));
        assert!(argv[2].contains("trap 'kill 0' EXIT"));
        assert!(argv[2].contains("exec 'sh' '-lc' 'echo hi'"));
    }

    #[test]
    fn unrestricted_command_never_gets_a_forwarder_even_with_sockets() {
        let config = effective(
            |session| {
                session.runtime.socat_path = Some(PathBuf::from("/usr/bin/socat"));
                session.runtime.http_proxy_port = Some(3128);
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            request(),
        );

        let argv = bridge::wrap_argv(&config).unwrap();

        assert_eq!(argv, vec!["sh", "-lc", "echo hi"]);
    }

    #[test]
    fn missing_socat_with_a_configured_socket_is_an_error() {
        let config = effective(
            |session| {
                session.runtime.http_proxy_port = Some(3128);
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            request().with_network_restriction(true),
        );

        let error = bridge::wrap_argv(&config).unwrap_err();

        assert!(matches!(
            error,
            SandboxError::MissingDependency {
                dependency: "socat",
                ..
            }
        ));
    }

    #[test]
    fn shell_quote_neutralises_embedded_quotes_and_metacharacters() {
        assert_eq!(bridge::shell_quote("plain"), "'plain'");
        assert_eq!(bridge::shell_quote("it's"), r"'it'\''s'");
        assert_eq!(
            bridge::shell_quote("$(rm -rf /); `id`"),
            "'$(rm -rf /); `id`'"
        );
    }

    #[test]
    fn a_command_cannot_break_out_of_the_forwarder_shell() {
        let config = effective(
            |session| {
                session.runtime.socat_path = Some(PathBuf::from("/usr/bin/socat"));
                session.runtime.http_proxy_port = Some(3128);
                session.runtime.http_proxy_socket = Some(PathBuf::from("/run/sbx/http.sock"));
            },
            CommandRequest::new("'; id > /tmp/pwned; '", BinShell::posix())
                .with_network_restriction(true),
        );

        let argv = bridge::wrap_argv(&config).unwrap();

        // The payload survives verbatim inside one quoted word — the
        // injected quote is escaped rather than closing the string.
        assert!(argv[2].contains(r"''\''; id > /tmp/pwned; '\'''"));
    }

    #[test]
    fn masked_file_under_a_write_root_is_reapplied() {
        let fs = FakeFs::new()
            .dir("/work")
            .file("/work/.npmrc")
            .file("/tmp/fake");
        let config = effective(
            |session| {
                session.filesystem.allow_write = vec![PathBuf::from("/work")];
                session.credentials.files = vec![(
                    PathBuf::from("/work/.npmrc"),
                    CredentialFileRule::Mask {
                        fake: PathBuf::from("/tmp/fake"),
                    },
                )];
            },
            request(),
        );

        let plan = build_mount_plan(&config, &fs);
        let bind_position = plan
            .ops
            .iter()
            .position(|op| matches!(op, MountOp::Bind { dest, .. } if dest == Path::new("/work")))
            .unwrap();
        let mask_positions: Vec<usize> = plan
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| op.dest() == Path::new("/work/.npmrc"))
            .map(|(index, _)| index)
            .collect();

        assert!(mask_positions.iter().any(|pos| *pos > bind_position));
    }

    #[test]
    fn per_command_read_denial_reaches_the_plan() {
        let fs = FakeFs::new().dir("/tmp").dir("/tmp/scratch");
        let config = effective(
            |_| {},
            request().with_read_rules(ReadRules {
                deny_only: vec![PathBuf::from("/tmp/scratch")],
                allow_within_deny: Vec::new(),
            }),
        );

        let plan = build_mount_plan(&config, &fs);

        assert!(plan.ops.contains(&MountOp::Tmpfs {
            dest: PathBuf::from("/tmp/scratch")
        }));
    }

    #[test]
    fn deny_within_allow_directory_hides_the_subtree() {
        let fs = FakeFs::new().dir("/work").dir("/work/secrets");
        let config = effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from("/work")],
            request().with_write_rules(WriteRules {
                allow_only: vec![PathBuf::from("/work")],
                deny_within_allow: vec![PathBuf::from("/work/secrets")],
            }),
        );

        let plan = build_mount_plan(&config, &fs);
        let bind_position = plan
            .ops
            .iter()
            .position(|op| matches!(op, MountOp::Bind { dest, .. } if dest == Path::new("/work")))
            .unwrap();
        let denial = plan
            .ops
            .iter()
            .position(|op| op.dest() == Path::new("/work/secrets"))
            .unwrap();

        assert!(
            denial > bind_position,
            "a write denial must land after the root it carves out of"
        );
    }

    #[test]
    fn masked_bind_argument_order_is_source_then_destination() {
        let op = MountOp::RoBind {
            src: PathBuf::from("/tmp/fake"),
            dest: PathBuf::from("/real"),
        };
        let mut args = Vec::new();
        op.push_args(&mut args);
        assert_eq!(args, vec!["--ro-bind", "/tmp/fake", "/real"]);
    }
}
