//! One process per stdio MCP server, however many sessions use it.
//!
//! Each session runs in its own process, and each used to start its own copy
//! of every configured stdio server: five sessions, five of each. The host is
//! a per-user process (`rebon __mcp-host`) that starts a server once and lets
//! every session that would have started the same one use it instead.
//!
//! * **Same server** — same name, command, arguments, working directory, the
//!   server's own `env` and the variables that decide which program the
//!   command is ([`ServerSpec::key`]). A server with no `cwd` runs where the
//!   session does, so each project gets its own.
//! * **Not shared** — a server whose config says `"shared": false`, Rebon's
//!   own servers (they answer for the session that started them), and every
//!   server in a process that has not installed a [`HostLauncher`] or runs with
//!   `REBON_MCP_NO_SHARE=1`. Those start the old way.
//! * **Stopping one** — [`stop_shared_server`] stops a server for every
//!   session at once. Stopped as [`StopReason::Disabled`], sessions drop it and
//!   its tools; as [`StopReason::Restart`], they keep it and attach to a fresh
//!   process the next time they use it — which is also what happens when a
//!   server or the host itself goes away on its own.
//! * **Finding the host** — it holds `host-v1.lock` under [`host_dir`] and
//!   publishes its loopback port and a token in `host-v1.json`. A session that
//!   finds none starts one and waits for it; losing candidates exit.

mod client;
mod host;
mod protocol;

pub use client::{
    install_launcher, list_shared_servers, shut_down_shared_host, stop_shared_server, HostLauncher,
};
pub use host::run_host;
pub use protocol::{SharedServerInfo, StopReason};

pub(crate) use client::{attach, should_share, AttachError, Attached};
pub(crate) use protocol::STOPPED_METHOD;

#[cfg(test)]
pub(crate) use client::{attach_in, spec_for};

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod test_support {
    //! A host running inside the test, at a directory of its own.

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::host::{run_host_in, HostTimings};
    use super::protocol::load_host_info;

    pub struct TestHost {
        pub dir: tempfile::TempDir,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl TestHost {
        pub async fn start(server_grace: Duration) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            let task = tokio::spawn(async move {
                run_host_in(
                    &path,
                    HostTimings {
                        idle_exit: Duration::from_secs(3600),
                        server_grace,
                        tick: Duration::from_millis(50),
                    },
                )
                .await
            });
            for _ in 0..200 {
                if load_host_info(dir.path()).is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                load_host_info(dir.path()).is_some(),
                "test host never came up"
            );
            Self { dir, task }
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        pub fn path_buf(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }
    }

    impl Drop for TestHost {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
}
