//! Starting a host: a Node process, or a Verve isolate in this process.
//!
//! Everything the supervisor does with a host goes through three byte streams
//! and a handle that ends it, so this is the one place that knows which kind it
//! started. `spawn` and `restart` both come here, and so start the same thing.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;

use crate::{host_command, HostConfig};

/// What runs a plugin host's JavaScript.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostEngine {
    /// `node cli.mjs`, a child process: the host as it has always run, and the
    /// only engine with a permission model for a contained host.
    #[default]
    Node,
    /// The same `cli.mjs` on Verve, an isolate inside this process: no Node to
    /// install. Not for a contained host yet — Verve has no permission model
    /// to confine one with.
    Verve,
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
    Node(Child),
    Verve(rebon_verve_host::VerveProcess),
}

impl HostProcess {
    /// Ends the host without asking it.
    pub(crate) fn start_kill(&mut self) {
        match self {
            Self::Node(child) => {
                let _ = child.start_kill();
            }
            Self::Verve(process) => process.start_kill(),
        }
    }

    /// Waits for the host to leave.
    pub(crate) async fn wait(&mut self) {
        match self {
            Self::Node(child) => {
                let _ = child.wait().await;
            }
            Self::Verve(process) => {
                process.wait().await;
            }
        }
    }
}

/// Starts the host `config` describes.
pub(crate) fn launch(config: &HostConfig) -> io::Result<Launched> {
    match config.engine {
        HostEngine::Node => {
            let mut child = host_command(config).spawn()?;
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");
            Ok(Launched {
                stdin: Box::new(stdin),
                stdout: Box::new(stdout),
                stderr: Box::new(stderr),
                process: HostProcess::Node(child),
            })
        }
        HostEngine::Verve => {
            // Refused rather than run unconfined: a launcher and Node's
            // permission flags are how a contained host is held, and Verve
            // would silently drop both.
            if config.launcher.is_some() || !config.node_args.is_empty() {
                return Err(io::Error::other(
                    "a contained plugin host cannot run on Verve yet: it has no permission model",
                ));
            }
            let host = rebon_verve_host::spawn(rebon_verve_host::VerveHostSpec {
                host_script: config.host_script.clone(),
                host_args: config.host_args.clone(),
                environment: config.environment.clone(),
                working_directory: config.working_directory.clone(),
                name: format!("plugin-host-{}", config.host_epoch),
                // Uncontained, as the check above made sure: the network a
                // trusted Node host has.
                network: rebon_verve_host::VerveHostSpec::trusted_network(),
            })?;
            Ok(Launched {
                stdin: Box::new(host.stdin),
                stdout: Box::new(host.stdout),
                stderr: Box::new(host.stderr),
                process: HostProcess::Verve(host.process),
            })
        }
    }
}
