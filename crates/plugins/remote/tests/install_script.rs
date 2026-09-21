//! Runs the generated install script through a real shell.
//!
//! The scripts in `rebon_plugin_remote::script` are strings until something
//! executes them, and the unit tests can only assert that the right
//! text is present. What they cannot catch is a quoting mistake that
//! makes `sh` parse the script differently than intended, a `set -eu`
//! interaction that aborts early, or a staging/rename sequence that
//! leaves the destination in the wrong shape.
//!
//! So this suite runs the push script — the one that reads its package
//! from stdin, which is exactly what ssh does on the far end — against
//! a real tarball in a real temporary directory. The "remote" is this
//! machine; the script does not know the difference.
//!
//! Skipped when `sh` or `tar` is missing, which is the normal case on
//! a bare Windows host without Git Bash.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rebon_plugin_remote::script::{
    install_script, parse_install_outcome, uninstall_script, PackageSource,
};
use rebon_plugin_remote::{InstallOutcome, RemoteHost};

fn tool(name: &str) -> Option<PathBuf> {
    let probe = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(probe).arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .to_string();
    Some(PathBuf::from(first))
}

/// `sh` and `tar`, or `None` if this host cannot run the scripts.
fn shell_and_tar() -> Option<(PathBuf, PathBuf)> {
    Some((tool("sh")?, tool("tar")?))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("rebon-remote-install-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Build a tarball shaped like a published `@rebon/cli-<os>-<cpu>`
/// package: `package/bin/{rebon,rg}`.
///
/// `sub` is relative to `work`, and tar runs *inside* `work` with
/// relative arguments throughout. GNU tar reads a leading `C:` as an
/// rmt remote host, so a Windows absolute path would be taken for a
/// machine to connect to rather than a file. Real remotes only ever
/// see POSIX paths, so keeping the test relative sidesteps a
/// difference that does not exist in production.
fn fake_package(tar: &Path, work: &Path, sub: &str, binary_name: &str, marker: &str) -> String {
    let bin = work.join(sub).join("package").join("bin");
    std::fs::create_dir_all(&bin).expect("package layout");
    std::fs::write(bin.join(binary_name), format!("#!/bin/sh\necho {marker}\n"))
        .expect("write binary");
    std::fs::write(bin.join("rg"), "#!/bin/sh\necho rg\n").expect("write rg");

    let relative = format!("{sub}/pkg.tgz");
    let output = Command::new(tar)
        .current_dir(work)
        .arg("-czf")
        .arg(&relative)
        .arg("-C")
        .arg(sub)
        .arg("package")
        .output()
        .expect("run tar");
    assert!(
        output.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    relative
}

/// Run a generated script under `sh`, from inside `work`, with
/// `stdin_file` (relative to `work`) piped in.
fn run(sh: &Path, work: &Path, script: &str, stdin_file: Option<&str>) -> (bool, String, String) {
    let mut command = Command::new(sh);
    command
        .current_dir(work)
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match stdin_file {
        Some(path) => {
            let file = std::fs::File::open(work.join(path)).expect("open package");
            command.stdin(Stdio::from(file));
        }
        None => {
            command.stdin(Stdio::null());
        }
    }
    let output = command.output().expect("run sh");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A host whose server directory is `root`, relative to the work dir.
fn host_rooted_at(root: &str) -> RemoteHost {
    let mut host = RemoteHost::from_target("prod", "deploy@host").expect("host");
    host.server_dir = Some(root.to_string());
    host
}

#[test]
fn a_push_install_unpacks_the_whole_bin_directory_into_a_versioned_path() {
    let Some((sh, tar)) = shell_and_tar() else {
        eprintln!("skipping: sh or tar not available");
        return;
    };
    let work = temp_dir("push");
    let host = host_rooted_at("server");
    let package = fake_package(&tar, &work, "pkg", "rebon", "v1");

    let script = install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, false);
    let (ok, stdout, stderr) = run(&sh, &work, &script, Some(&package));
    assert!(ok, "script failed:\nstdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_install_outcome(&stdout), InstallOutcome::Installed);

    // The version directory *is* the package's bin directory, so
    // ripgrep lands beside the binary — Grep needs it.
    let root = work.join("server");
    assert!(
        root.join("0.15.0").join("rebon").is_file(),
        "binary missing"
    );
    assert!(root.join("0.15.0").join("rg").is_file(), "ripgrep missing");
    // Nothing is left staged.
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .expect("read root")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "0.15.0")
        .collect();
    assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
}

#[test]
fn a_second_install_skips_without_force_and_replaces_with_it() {
    let Some((sh, tar)) = shell_and_tar() else {
        eprintln!("skipping: sh or tar not available");
        return;
    };
    let work = temp_dir("reinstall");
    let host = host_rooted_at("server");
    let root = work.join("server");

    let first = fake_package(&tar, &work, "a", "rebon", "v1");
    let script = install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, false);
    let (ok, stdout, stderr) = run(&sh, &work, &script, Some(&first));
    assert!(ok, "first install failed:\n{stderr}");
    assert_eq!(parse_install_outcome(&stdout), InstallOutcome::Installed);

    // Without --force the script must not touch what is there.
    let second = fake_package(&tar, &work, "b", "rebon", "v2");
    let (ok, stdout, stderr) = run(&sh, &work, &script, Some(&second));
    assert!(ok, "skip run failed:\n{stderr}");
    assert_eq!(
        parse_install_outcome(&stdout),
        InstallOutcome::AlreadyPresent
    );
    let installed = std::fs::read_to_string(root.join("0.15.0").join("rebon")).unwrap();
    assert!(installed.contains("v1"), "the skip overwrote the build");

    let forced = install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, true);
    let (ok, stdout, stderr) = run(&sh, &work, &forced, Some(&second));
    assert!(ok, "forced install failed:\n{stderr}");
    assert_eq!(parse_install_outcome(&stdout), InstallOutcome::Installed);
    let installed = std::fs::read_to_string(root.join("0.15.0").join("rebon")).unwrap();
    assert!(
        installed.contains("v2"),
        "--force did not replace the build"
    );
}

#[test]
fn two_versions_coexist_so_an_upgrade_cannot_pull_the_floor_out() {
    // A live remote session is executing out of its version directory.
    // Installing another version must leave that one alone.
    let Some((sh, tar)) = shell_and_tar() else {
        eprintln!("skipping: sh or tar not available");
        return;
    };
    let work = temp_dir("versions");
    let host = host_rooted_at("server");
    let root = work.join("server");

    let old = fake_package(&tar, &work, "old", "rebon", "old-build");
    let new = fake_package(&tar, &work, "new", "rebon", "new-build");
    let (ok, _, stderr) = run(
        &sh,
        &work,
        &install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, false),
        Some(&old),
    );
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = run(
        &sh,
        &work,
        &install_script(&host, "0.16.0", "rebon", &PackageSource::Stdin, false),
        Some(&new),
    );
    assert!(ok, "{stderr}");

    assert!(std::fs::read_to_string(root.join("0.15.0").join("rebon"))
        .unwrap()
        .contains("old-build"));
    assert!(std::fs::read_to_string(root.join("0.16.0").join("rebon"))
        .unwrap()
        .contains("new-build"));
}

#[test]
fn an_empty_transfer_fails_instead_of_installing_nothing() {
    // A download that dies halfway can still exit 0. Untarring the
    // result would leave an empty directory the script would then
    // report as a success.
    let Some((sh, _tar)) = shell_and_tar() else {
        eprintln!("skipping: sh or tar not available");
        return;
    };
    let work = temp_dir("empty");
    let host = host_rooted_at("server");
    std::fs::write(work.join("empty.tgz"), b"").expect("write empty");

    let script = install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, false);
    let (ok, stdout, stderr) = run(&sh, &work, &script, Some("empty.tgz"));
    assert!(!ok, "an empty package must not install");
    assert_eq!(parse_install_outcome(&stdout), InstallOutcome::Unknown);
    assert!(stderr.contains("empty file"), "{stderr}");
    assert!(
        !work.join("server").join("0.15.0").exists(),
        "a failed install left a version behind"
    );
}

#[test]
fn a_package_without_the_expected_binary_is_rejected() {
    let Some((sh, tar)) = shell_and_tar() else {
        eprintln!("skipping: sh or tar not available");
        return;
    };
    let work = temp_dir("wrong-binary");
    let host = host_rooted_at("server");
    // The case this guards is a package for the wrong platform — a
    // win32 tarball pushed at a linux host carries `rebon.exe`, and
    // installing it would leave a version directory that connect-time
    // cannot run. The stand-in is named something else entirely
    // because MSYS resolves `test -f rebon` against `rebon.exe`, so on
    // a Windows test host the realistic name would appear to be
    // present. A real remote has no such fallback; the guard under
    // test is the same either way.
    let package = fake_package(&tar, &work, "pkg", "rebon-for-another-os", "wrong");

    let script = install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, false);
    let (ok, _stdout, stderr) = run(&sh, &work, &script, Some(&package));
    assert!(!ok, "the wrong package must not install");
    assert!(stderr.contains("did not contain"), "{stderr}");
    assert!(!work.join("server").join("0.15.0").exists());
}

#[test]
fn a_server_directory_with_a_space_survives_install_and_uninstall() {
    // The quoting boundary, end to end: the path reaches `sh` as one
    // word or the script silently operates on the wrong directory.
    let Some((sh, tar)) = shell_and_tar() else {
        eprintln!("skipping: sh or tar not available");
        return;
    };
    let work = temp_dir("spaces");
    let host = host_rooted_at("rebon servers");
    let root = work.join("rebon servers");
    let package = fake_package(&tar, &work, "pkg", "rebon", "spaced");

    let script = install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, false);
    let (ok, stdout, stderr) = run(&sh, &work, &script, Some(&package));
    assert!(ok, "install failed:\n{stderr}");
    assert_eq!(parse_install_outcome(&stdout), InstallOutcome::Installed);
    assert!(root.join("0.15.0").join("rebon").is_file());

    let (ok, stdout, stderr) = run(&sh, &work, &uninstall_script(&host, Some("0.15.0")), None);
    assert!(ok, "uninstall failed:\n{stderr}");
    assert!(stdout.contains(rebon_plugin_remote::exec::OK_MARKER));
    assert!(
        !root.join("0.15.0").exists(),
        "uninstall left the version behind"
    );
    // Only that version was removed; the root itself stays.
    assert!(root.exists());
}

#[test]
fn uninstall_all_removes_the_whole_root() {
    let Some((sh, tar)) = shell_and_tar() else {
        eprintln!("skipping: sh or tar not available");
        return;
    };
    let work = temp_dir("uninstall-all");
    let host = host_rooted_at("server");
    let root = work.join("server");
    let package = fake_package(&tar, &work, "pkg", "rebon", "v1");
    let (ok, _, stderr) = run(
        &sh,
        &work,
        &install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, false),
        Some(&package),
    );
    assert!(ok, "{stderr}");

    let (ok, stdout, stderr) = run(&sh, &work, &uninstall_script(&host, None), None);
    assert!(ok, "uninstall failed:\n{stderr}");
    assert!(stdout.contains(rebon_plugin_remote::exec::OK_MARKER));
    assert!(!root.exists(), "the root should be gone");
}
