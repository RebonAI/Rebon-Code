//! Starting a host: a Verve isolate in this process, or a `rebon-verve`
//! process of its own for a host something confines.
//!
//! Everything the supervisor does with a host goes through three byte streams
//! and a handle that ends it, so this is the one place that knows which kind it
//! started. `spawn` and `restart` both come here, and so start the same thing.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;

use crate::{host_command, HostConfig};

/// Where a plugin host's JavaScript runs. Verve either way; what differs is
/// whether the host shares this process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostEngine {
    /// An isolate inside this process: the trusted plane's shared host and a
    /// session's loop host.
    #[default]
    Isolate,
    /// `rebon-verve cli.mjs`, a process of its own, under Verve's permission
    /// flags and whatever OS sandbox wraps it: a contained host.
    Process,
}

pub(crate) type HostInput = Box<dyn AsyncWrite + Send + Unpin>;
pub(crate) type HostOutput = Box<dyn AsyncRead + Send + Unpin>;

/// A started host: its streams, and what ends it.
pub(crate) struct Launched {
    pub(crate) stdin: HostInput,
    pub(crate) stdout: HostOutput,
    pub(crate) stderr: HostOutput,
    pub(crate) process: HostProcess,
}

/// The host as something that can be ended and waited for.
pub(crate) enum HostProcess {
    /// A runner process of its own (boxed: a `Child` is far larger than
    /// the isolate's handle).
    Process(Box<Child>),
    Isolate(rebon_verve_host::VerveProcess),
}

impl HostProcess {
    /// Ends the host without asking it.
    pub(crate) fn start_kill(&mut self) {
        match self {
            Self::Process(child) => {
                let _ = child.start_kill();
            }
            Self::Isolate(process) => process.start_kill(),
        }
    }

    /// Waits for the host to leave.
    pub(crate) async fn wait(&mut self) {
        match self {
            Self::Process(child) => {
                let _ = child.wait().await;
            }
            Self::Isolate(process) => {
                process.wait().await;
            }
        }
    }
}

/// Starts the host `config` describes.
pub(crate) fn launch(config: &HostConfig) -> io::Result<Launched> {
    match config.engine {
        HostEngine::Process => {
            let mut child = host_command(config).spawn()?;
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");
            Ok(Launched {
                stdin: Box::new(stdin),
                stdout: Box::new(stdout),
                stderr: Box::new(stderr),
                process: HostProcess::Process(Box::new(child)),
            })
        }
        HostEngine::Isolate => {
            // Refused rather than run unconfined: a launcher and the runner's
            // permission flags are how a contained host is held, and an
            // isolate in this process would silently drop both.
            if config.launcher.is_some() || !config.runner_args.is_empty() {
                return Err(io::Error::other(
                    "a contained plugin host runs as a process of its own, not in this one",
                ));
            }
            let host = rebon_verve_host::spawn(rebon_verve_host::VerveHostSpec {
                host_script: config.host_script.clone(),
                host_args: config.host_args.clone(),
                environment: config.environment.clone(),
                working_directory: config.working_directory.clone(),
                name: format!("plugin-host-{}", config.host_epoch),
                // Uncontained, as the check above made sure: everything, as a
                // trusted host has.
                permissions: rebon_verve_host::VerveHostSpec::trusted(),
            })?;
            Ok(Launched {
                stdin: Box::new(host.stdin),
                stdout: Box::new(host.stdout),
                stderr: Box::new(host.stderr),
                process: HostProcess::Isolate(host.process),
            })
        }
    }
}
