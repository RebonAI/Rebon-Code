//! 显式执行 npm 更新，不受启动通知的忽略偏好影响。

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{ensure, Context};
use rebon_tools_core::process_tree::ProcessTreeGuard;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::check::UpdateCheckResult;
use crate::installation::{current_npm_installation, NpmInstallation};
use crate::updater::{update_lock_path, UpdateLock};

// npm 的网络请求或生命周期脚本可能挂起，不能无限占用更新锁。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
// 仅保留诊断尾部，避免 npm 的长输出占满内存。
const OUTPUT_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualUpdateResult {
    Installed {
        current_version: String,
        latest_version: String,
    },
    UpToDate {
        current_version: String,
    },
}

/// 丢弃此 future 会终止 npm 进程树并释放更新锁；输出不会写入 TUI 终端。
pub async fn install_update(
    on_progress: impl Fn(String) + Send + Sync,
) -> anyhow::Result<ManualUpdateResult> {
    on_progress("Validating npm installation...".into());
    install_update_with(
        current_npm_installation(),
        &update_lock_path(&rebon_config::config_home_dir()),
        crate::check::check_for_manual_update,
        run_npm,
        INSTALL_TIMEOUT,
        &on_progress,
    )
    .await
}

async fn install_update_with<C, CF, R, RF>(
    installation: anyhow::Result<NpmInstallation>,
    lock_path: &Path,
    check: C,
    runner: R,
    timeout: Duration,
    progress: &(impl Fn(String) + Send + Sync),
) -> anyhow::Result<ManualUpdateResult>
where
    C: FnOnce() -> CF,
    CF: Future<Output = anyhow::Result<UpdateCheckResult>>,
    R: FnOnce(NpmCommand) -> RF,
    RF: Future<Output = anyhow::Result<()>>,
{
    let install = installation?;
    let _lock = UpdateLock::acquire(lock_path)?;
    progress("Checking for updates...".into());
    let checked = check().await?;
    ensure!(
        checked.package_name == "@rebon/cli",
        "refusing to install a package other than the validated @rebon/cli wrapper"
    );
    let current =
        semver::Version::parse(&checked.current_version).context("invalid current version")?;
    let latest = exact_version(&checked.latest_version)?;
    if current.cmp_precedence(&latest).is_ge() {
        return Ok(ManualUpdateResult::UpToDate {
            current_version: checked.current_version,
        });
    }
    // 固定已查询的版本，避免 dist-tag 在检查与安装之间变化。
    let command = npm_command(&install, &latest)?;
    progress(format!(
        "Installing @rebon/cli@{} ({})...",
        checked.latest_version,
        if install.global {
            "npm global"
        } else {
            "npm local"
        }
    ));
    tokio::time::timeout(timeout, runner(command))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "npm update timed out after {} seconds; the installer was cancelled",
                timeout.as_secs()
            )
        })??;
    verify_installed_version(&install, &checked.latest_version)?;
    Ok(ManualUpdateResult::Installed {
        current_version: checked.current_version,
        latest_version: checked.latest_version,
    })
}

fn verify_installed_version(install: &NpmInstallation, version: &str) -> anyhow::Result<()> {
    let package: serde_json::Value =
        serde_json::from_slice(&std::fs::read(install.wrapper.join("package.json"))?)?;
    ensure!(
        package["name"] == "@rebon/cli" && package["version"] == version,
        "npm exited successfully but the installed @rebon/cli version does not match {version}"
    );
    let nested = install
        .wrapper
        .join("node_modules")
        .join(&install.platform_package);
    let hoisted = install
        .wrapper
        .parent()
        .and_then(Path::parent)
        .context("npm wrapper layout changed during installation")?
        .join(&install.platform_package);
    let platform = if nested.join("package.json").is_file() {
        nested
    } else {
        hoisted
    };
    let bytes = std::fs::read(platform.join("package.json"))
        .context("npm updated the wrapper but its optional platform package is missing; installation is incomplete")?;
    let installed: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(installed["name"] == install.platform_package && installed["version"] == version,
        "npm updated the wrapper but its platform package does not match {version}; installation is incomplete");
    let payload = platform.join(if install.platform_package.contains("-win32-") {
        "payload/rebon.exe"
    } else {
        "bin/rebon"
    });
    ensure!(
        payload.is_file(),
        "npm platform payload is missing; installation is incomplete"
    );
    Ok(())
}

fn exact_version(version: &str) -> anyhow::Result<semver::Version> {
    let parsed = semver::Version::parse(version)
        .context("npm update target must be an exact semantic version")?;
    ensure!(
        version == parsed.to_string(),
        "npm update target must be a canonical exact version"
    );
    Ok(parsed)
}

#[derive(Debug)]
struct NpmCommand {
    program: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
}

fn npm_command(install: &NpmInstallation, version: &semver::Version) -> anyhow::Result<NpmCommand> {
    let npm = find_npm_cli(&install.node)?;
    let mut args = vec![
        npm.into_os_string(),
        "install".into(),
        if install.global {
            "--global=true".into()
        } else {
            "--global=false".into()
        },
        "--prefix".into(),
        install.prefix.clone().into_os_string(),
        "--no-audit".into(),
        "--no-fund".into(),
        "--no-progress".into(),
        "--color=false".into(),
        "--include=optional".into(),
        "--dry-run=false".into(),
        "--package-lock-only=false".into(),
    ];
    if !install.global {
        args.extend(["--workspaces=false".into(), "--include=dev".into()]);
    }
    args.extend(["--".into(), format!("@rebon/cli@{version}").into()]);
    Ok(NpmCommand {
        program: install.node.clone(),
        args,
        cwd: install.prefix.clone(),
    })
}

fn validated_npm_cli(path: &Path) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).ok()?;
    if path.file_name()? != "npm-cli.js" || path.parent()?.file_name()? != "bin" {
        return None;
    }
    let root = path.parent()?.parent()?;
    let package: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("package.json")).ok()?).ok()?;
    (package["name"] == "npm" && package["bin"]["npm"] == "bin/npm-cli.js").then_some(path)
}

fn find_npm_cli(node: &Path) -> anyhow::Result<PathBuf> {
    let dir = node
        .parent()
        .context("Node executable has no parent directory")?;
    // 优先使用当前 Node 随附的 npm，避免切换到另一套运行时。
    for path in [
        dir.join("node_modules/npm/bin/npm-cli.js"),
        dir.join("../lib/node_modules/npm/bin/npm-cli.js"),
    ] {
        if let Some(cli) = validated_npm_cli(&path) {
            return Ok(cli);
        }
    }
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path).filter(|dir| dir.is_absolute()) {
            candidates.push(dir.join("npm"));
            candidates.push(dir.join("node_modules/npm/bin/npm-cli.js"));
            candidates.push(dir.join("../lib/node_modules/npm/bin/npm-cli.js"));
        }
    }
    candidates.iter().find_map(|path| validated_npm_cli(path))
        .context("Cannot find a verified npm CLI beside Node or on PATH. Install npm for this Node runtime, then retry. No shell/npm.cmd fallback was run.")
}

async fn read_tail(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > OUTPUT_LIMIT {
            tail.drain(..tail.len() - OUTPUT_LIMIT);
        }
    }
}

fn output_text(bytes: &[u8]) -> String {
    // 关闭颜色仍不能保证 npm 的诊断中没有终端控制字符。
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control() || matches!(*c, '\n' | '\t'))
        .collect::<String>()
        .trim()
        .to_string()
}

async fn run_npm(plan: NpmCommand) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_remove("REBON_NPM_INSTALL");
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW，防止安装脚本弹出窗口。
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to start npm installer: {err}"))?;
    #[cfg(windows)]
    let _tree = ProcessTreeGuard::for_raw_handle(
        child
            .raw_handle()
            .context("npm child has no process handle")?,
        false,
    )
    .context("failed to contain npm process tree")?;
    #[cfg(unix)]
    let _tree =
        ProcessTreeGuard::for_process_group(child.id().context("npm child has no process id")?)?;
    let stdout = child.stdout.take().context("npm stdout pipe missing")?;
    let stderr = child.stderr.take().context("npm stderr pipe missing")?;
    let (status, stdout, stderr) =
        tokio::try_join!(child.wait(), read_tail(stdout), read_tail(stderr))
            .context("failed to capture npm installer output")?;
    ensure!(
        status.success(),
        "npm update failed ({status})\nstdout:\n{}\nstderr:\n{}",
        output_text(&stdout),
        output_text(&stderr)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::UpdateDecision;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fixture(root: &Path, global: bool) -> NpmInstallation {
        let prefix = root.join("prefix with spaces");
        let wrapper = prefix.join("node_modules/@rebon/cli");
        std::fs::create_dir_all(&wrapper).unwrap();
        std::fs::write(
            wrapper.join("package.json"),
            r#"{"name":"@rebon/cli","version":"1.0.0"}"#,
        )
        .unwrap();
        let node = root.join("node runtime/node.exe");
        let npm = node.parent().unwrap().join("node_modules/npm");
        std::fs::create_dir_all(npm.join("bin")).unwrap();
        std::fs::write(
            npm.join("package.json"),
            r#"{"name":"npm","bin":{"npm":"bin/npm-cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(npm.join("bin/npm-cli.js"), "").unwrap();
        NpmInstallation {
            wrapper,
            prefix,
            node,
            global,
            platform_package: "@rebon/cli-linux-x64".into(),
        }
    }

    fn checked(latest: &str) -> UpdateCheckResult {
        UpdateCheckResult {
            current_version: "1.0.0".into(),
            latest_version: latest.into(),
            package_name: "@rebon/cli".into(),
            source: "test".into(),
            command: "NEVER EXECUTE THIS SHELL STRING".into(),
            checked_at: std::time::SystemTime::UNIX_EPOCH,
            decision: UpdateDecision::SkippedByMinimumVersion,
        }
    }

    #[test]
    fn npm_argv_pins_version_scope_and_prefix_without_a_shell() {
        let root = tempfile::tempdir().unwrap();
        for global in [false, true] {
            let install = fixture(root.path(), global);
            let command = npm_command(&install, &exact_version("2.0.0-beta.1").unwrap()).unwrap();
            assert_eq!(command.program, install.node);
            assert_eq!(command.cwd, install.prefix);
            assert_eq!(command.args[1], "install");
            assert_eq!(
                command.args[2],
                if global {
                    "--global=true"
                } else {
                    "--global=false"
                }
            );
            assert_eq!(command.args[3], "--prefix");
            assert_eq!(command.args[4], install.prefix.as_os_str());
            assert_eq!(command.args.last().unwrap(), "@rebon/cli@2.0.0-beta.1");
            assert!(!command
                .args
                .iter()
                .any(|arg| arg == "latest" || arg == "npm.cmd"));
        }
        for version in [
            "latest",
            "--global",
            "1.2.3;evil",
            "file:evil",
            "https://evil",
            "1.2.3 evil",
            "v1.2.3",
            "1.2.3\n",
        ] {
            assert!(exact_version(version).is_err(), "{version:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn latest_never_runs_npm_and_releases_lock() {
        let root = tempfile::tempdir().unwrap();
        let lock = root.path().join("update.lock");
        let result = install_update_with(
            Ok(fixture(root.path(), false)),
            &lock,
            || async { Ok(checked("1.0.0")) },
            |_| async { panic!("must not execute npm") },
            INSTALL_TIMEOUT,
            &|_| {},
        )
        .await
        .unwrap();
        assert!(matches!(result, ManualUpdateResult::UpToDate { .. }));
        assert!(!lock.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_npm_and_contended_lock_never_query_or_execute() {
        let root = tempfile::tempdir().unwrap();
        let lock = root.path().join("update.lock");
        for installation in [
            Err(anyhow::anyhow!("not npm")),
            Ok(fixture(root.path(), true)),
        ] {
            let _held = UpdateLock::acquire(&lock).unwrap();
            let result = install_update_with(
                installation,
                &lock,
                || async { panic!("must not query") },
                |_| async { panic!("must not install") },
                INSTALL_TIMEOUT,
                &|_| {},
            )
            .await;
            assert!(result.is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn success_uses_checked_target_even_when_notice_was_dismissed() {
        let root = tempfile::tempdir().unwrap();
        let install = fixture(root.path(), false);
        let wrapper = install.wrapper.clone();
        let result = install_update_with(
            Ok(install),
            &root.path().join("update.lock"),
            || async { Ok(checked("2.0.0")) },
            |command| async move {
                assert_eq!(command.args.last().unwrap(), "@rebon/cli@2.0.0");
                std::fs::write(
                    wrapper.join("package.json"),
                    r#"{"name":"@rebon/cli","version":"2.0.0"}"#,
                )?;
                let platform = wrapper.join("node_modules/@rebon/cli-linux-x64");
                std::fs::create_dir_all(platform.join("bin"))?;
                std::fs::write(
                    platform.join("package.json"),
                    r#"{"name":"@rebon/cli-linux-x64","version":"2.0.0"}"#,
                )?;
                std::fs::write(platform.join("bin/rebon"), "binary")?;
                Ok(())
            },
            INSTALL_TIMEOUT,
            &|_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            ManualUpdateResult::Installed {
                current_version: "1.0.0".into(),
                latest_version: "2.0.0".into()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn installer_failure_and_timeout_release_lock_without_claiming_success() {
        let root = tempfile::tempdir().unwrap();
        let lock = root.path().join("update.lock");
        let error = install_update_with(
            Ok(fixture(root.path(), true)),
            &lock,
            || async { Ok(checked("2.0.0")) },
            |_| async { anyhow::bail!("npm stderr: EACCES permission denied") },
            INSTALL_TIMEOUT,
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("EACCES"));
        assert!(!lock.exists());
        struct DropFlag<'a>(&'a AtomicBool);
        impl Drop for DropFlag<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let cancelled = AtomicBool::new(false);
        let error = install_update_with(
            Ok(fixture(root.path(), true)),
            &lock,
            || async { Ok(checked("2.0.0")) },
            |_| async {
                let _guard = DropFlag(&cancelled);
                std::future::pending::<anyhow::Result<()>>().await
            },
            Duration::from_millis(10),
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(!lock.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malicious_target_or_package_never_executes() {
        let root = tempfile::tempdir().unwrap();
        let mut malicious = checked("2.0.0");
        malicious.package_name = "@evil/package".into();
        for check in [malicious, checked("--prefix=evil")] {
            assert!(install_update_with(
                Ok(fixture(root.path(), false)),
                &root.path().join("update.lock"),
                || async { Ok(check) },
                |_| async { panic!("unsafe input ran npm") },
                INSTALL_TIMEOUT,
                &|_| {}
            )
            .await
            .is_err());
        }
    }

    #[test]
    fn wrapper_only_update_is_not_a_successful_install() {
        let root = tempfile::tempdir().unwrap();
        let install = fixture(root.path(), true);
        std::fs::write(
            install.wrapper.join("package.json"),
            r#"{"name":"@rebon/cli","version":"2.0.0"}"#,
        )
        .unwrap();
        assert!(verify_installed_version(&install, "2.0.0")
            .unwrap_err()
            .to_string()
            .contains("platform package is missing"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn zero_exit_without_changing_version_is_not_success() {
        let root = tempfile::tempdir().unwrap();
        let error = install_update_with(
            Ok(fixture(root.path(), true)),
            &root.path().join("update.lock"),
            || async { Ok(checked("2.0.0")) },
            |_| async { Ok(()) },
            INSTALL_TIMEOUT,
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn aborted_update_drops_runner_and_releases_lock() {
        let root = tempfile::tempdir().unwrap();
        let install = fixture(root.path(), true);
        let lock = root.path().join("update.lock");
        let task_lock = lock.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            install_update_with(
                Ok(install),
                &task_lock,
                || async { Ok(checked("2.0.0")) },
                |_| async {
                    started.send(()).unwrap();
                    std::future::pending::<anyhow::Result<()>>().await
                },
                INSTALL_TIMEOUT,
                &|_| {},
            )
            .await
        });
        entered.await.unwrap();
        assert!(lock.exists());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!lock.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_failure_includes_os_diagnostic() {
        let root = tempfile::tempdir().unwrap();
        let error = run_npm(NpmCommand {
            program: root.path().join("does-not-exist"),
            args: Vec::new(),
            cwd: root.path().to_path_buf(),
        })
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .starts_with("failed to start npm installer: "));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn process_runner_captures_success_and_nonzero_exit_without_installing() {
        let root = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        run_npm(NpmCommand {
            program: executable.clone(),
            args: vec!["--list".into()],
            cwd: root.path().to_path_buf(),
        })
        .await
        .unwrap();
        let error = run_npm(NpmCommand {
            program: executable,
            args: vec!["--invalid-updater-test-argument".into()],
            cwd: root.path().to_path_buf(),
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("npm update failed"));
        assert!(error.to_string().contains("invalid-updater-test-argument"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn captures_bounded_output_and_filters_terminal_controls() {
        let bytes = vec![b'x'; OUTPUT_LIMIT * 2];
        let tail = read_tail(bytes.as_slice()).await.unwrap();
        assert_eq!(tail.len(), OUTPUT_LIMIT);
        assert_eq!(
            output_text(b"permission\x1b denied\r\n"),
            "permission denied"
        );
    }
}
