//! The plugin host's JavaScript, run on Verve in this process.
//!
//! On Node the plugin host is a process: `node cli.mjs --loader <module>`, its
//! protocol on stdin and stdout, its diagnostics on stderr. Here it is a Verve
//! isolate — a JavaScript runtime on a thread of its own — running the same
//! `runHost` from the same `cli.mjs`, and [`spawn`] hands back the same three
//! streams as byte pipes. The supervisor reads and writes them exactly as it
//! does a child's, so the ledger, the epochs and every caller above it do not
//! know which one they have.
//!
//! What stands in for the process:
//!
//! - **stdin** goes to the isolate in chunks, as jobs; the host's `read()`
//!   promise is answered with each, and with `null` once the pipe is closed.
//! - **stdout** is every frame the host writes, in order. It closes when the
//!   host's loop returns, which is what the supervisor reads as a process
//!   leaving.
//! - **stderr** is the host's diagnostics, and whatever plugins wrote to the
//!   console or `process.stderr`, one line each.
//! - **exit**: the code `runHost` returned; or [`HostExit::Killed`]; or
//!   [`HostExit::Died`] when the isolate itself panicked.
//!
//! Killing interrupts whatever script is running — uncatchably — and lets the
//! isolate go: its thread ends once that is done. Nothing a plugin started
//! outlives it.
//!
//! What this is not, yet: a sandbox. A contained host (its own files, no
//! processes, the network it was granted) still runs on Node, which has the
//! permission model; Verve's is the next step.

mod guest;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{watch, Notify};
use verve::isolate::{Health, Isolate, Options};

use guest::{HostEvent, HostGuest};

/// How big a pipe's buffer is before a writer waits for its reader.
const PIPE_BYTES: usize = 1 << 20;

/// The stack an isolate runs on: Boa recurses deeply, and a bundled plugin
/// parsed in a dev build needs room (the README of Verve asks for 16 MB or
/// more).
const ISOLATE_STACK: usize = 64 * 1024 * 1024;

/// What a Verve host is started with: the parts of Node's command line that
/// mean anything without Node.
#[derive(Clone, Debug)]
pub struct VerveHostSpec {
    /// The host's `cli.mjs`. The entry is loaded beside it and imports it.
    pub host_script: PathBuf,
    /// The host's own arguments: `--loader <module>`.
    pub host_args: Vec<String>,
    /// The whole environment `process.env` starts with; `None` is this
    /// process's own.
    pub environment: Option<Vec<(OsString, OsString)>>,
    /// `process.cwd()`.
    pub working_directory: PathBuf,
    /// The isolate thread's name, for whoever reads a thread list.
    pub name: String,
    /// What the host and the workers it starts may reach: files, workers, and
    /// the network (`fetch`, and Node's `net`, `http`, `tls` and `dgram`).
    pub permissions: verve::Permissions,
}

impl VerveHostSpec {
    /// What a trusted host has on Node: every file, workers, any host and
    /// port, listening and DNS. What an uncontained plugin plane runs with.
    pub fn trusted() -> verve::Permissions {
        verve::Permissions::all()
    }
}

/// How a host ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostExit {
    /// The host's loop returned this code, as `process.exitCode` would be.
    Code(i32),
    /// [`VerveProcess::start_kill`] ended it.
    Killed,
    /// The isolate panicked, or could not be made: its message.
    Died(String),
}

impl HostExit {
    /// The code a process with this ending would have exited with.
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Code(code) => Some(*code),
            Self::Killed | Self::Died(_) => None,
        }
    }

    pub fn success(&self) -> bool {
        self == &Self::Code(0)
    }
}

/// A running host: its three pipes, and the handle that ends it.
pub struct VerveHost {
    /// Where the host's input is written.
    pub stdin: DuplexStream,
    /// The host's protocol output.
    pub stdout: DuplexStream,
    /// The host's diagnostics.
    pub stderr: DuplexStream,
    pub process: VerveProcess,
}

/// What a supervisor holds of a host in place of its child process.
#[derive(Clone)]
pub struct VerveProcess {
    control: Arc<Control>,
}

struct Control {
    /// The isolate, until the host ends or is killed; dropping it lets the
    /// isolate's thread finish.
    isolate: Mutex<Option<Isolate<HostGuest>>>,
    /// Woken by the isolate when it has something for the embedder, and by a
    /// kill.
    news: Notify,
    exit: watch::Sender<Option<HostExit>>,
}

impl Control {
    /// Records how the host ended, the first time only, and lets the isolate
    /// go.
    fn finish(&self, exit: HostExit) {
        self.exit.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(exit);
            true
        });
        let isolate = self.isolate.lock().expect("verve isolate poisoned").take();
        drop(isolate);
        self.news.notify_one();
    }

    fn ended(&self) -> bool {
        self.exit.borrow().is_some()
    }
}

impl VerveProcess {
    /// Ends the host: interrupts what it is running and lets the isolate go.
    /// Returns at once; [`VerveProcess::wait`] says when it is over.
    pub fn start_kill(&self) {
        if let Some(isolate) = self
            .control
            .isolate
            .lock()
            .expect("verve isolate poisoned")
            .as_ref()
        {
            isolate.interrupt();
        }
        self.control.finish(HostExit::Killed);
    }

    /// Waits for the host to end.
    pub async fn wait(&self) -> HostExit {
        let mut exit = self.control.exit.subscribe();
        loop {
            if let Some(exit) = exit.borrow_and_update().clone() {
                return exit;
            }
            if exit.changed().await.is_err() {
                return HostExit::Died("the host's handle is gone".into());
            }
        }
    }

    /// How the host ended, if it has.
    pub fn try_exit(&self) -> Option<HostExit> {
        self.control.exit.borrow().clone()
    }
}

/// Starts a host on an isolate of its own. Must be called inside a Tokio
/// runtime: the pipes are pumped by tasks on it.
pub fn spawn(spec: VerveHostSpec) -> std::io::Result<VerveHost> {
    let (stdin, stdin_inside) = tokio::io::duplex(PIPE_BYTES);
    let (stdout_inside, stdout) = tokio::io::duplex(PIPE_BYTES);
    let (stderr_inside, stderr) = tokio::io::duplex(PIPE_BYTES);

    let (exit, _) = watch::channel(None);
    let control = Arc::new(Control {
        isolate: Mutex::new(None),
        news: Notify::new(),
        exit,
    });

    let woken = Arc::clone(&control);
    let options = Options {
        name: spec.name.clone(),
        stack_size: ISOLATE_STACK,
        ..Options::default()
    };
    let isolate = Isolate::spawn(
        options,
        move || HostGuest::start(&spec),
        move || woken.news.notify_one(),
    )?;
    *control.isolate.lock().expect("verve isolate poisoned") = Some(isolate);

    tokio::spawn(pump_input(stdin_inside, Arc::clone(&control)));
    tokio::spawn(pump_output(
        stdout_inside,
        stderr_inside,
        Arc::clone(&control),
    ));

    Ok(VerveHost {
        stdin,
        stdout,
        stderr,
        process: VerveProcess { control },
    })
}

/// Hands what is written to the host's stdin to the isolate, chunk by chunk,
/// then its end.
async fn pump_input(mut stdin: DuplexStream, control: Arc<Control>) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = stdin.read(&mut buffer).await;
        let chunk = match read {
            Ok(0) | Err(_) => None,
            Ok(read) => Some(buffer[..read].to_vec()),
        };
        let done = chunk.is_none();
        {
            let isolate = control.isolate.lock().expect("verve isolate poisoned");
            let Some(isolate) = isolate.as_ref() else {
                return;
            };
            isolate.send(move |guest, _| guest.input(chunk));
        }
        if done {
            return;
        }
    }
}

/// Moves what the isolate says to the host's stdout and stderr, until it
/// exits, dies or is killed — and then closes both, which is how the
/// supervisor learns the host is gone.
async fn pump_output(mut stdout: DuplexStream, mut stderr: DuplexStream, control: Arc<Control>) {
    loop {
        control.news.notified().await;
        let (events, health) = {
            let isolate = control.isolate.lock().expect("verve isolate poisoned");
            match isolate.as_ref() {
                Some(isolate) => (isolate.take_events(), isolate.health()),
                None => (Vec::new(), Health::Alive),
            }
        };
        for event in events {
            match event {
                HostEvent::Stdout(bytes) => {
                    if stdout.write_all(&bytes).await.is_err() {
                        // The supervisor stopped reading: nobody is left to
                        // serve.
                        control.finish(HostExit::Killed);
                    }
                }
                HostEvent::Stderr(bytes) => {
                    // A diagnostics reader that went away is not a reason to
                    // stop the protocol.
                    let _ = stderr.write_all(&bytes).await;
                }
                HostEvent::Exit(code) => control.finish(HostExit::Code(code)),
            }
        }
        if let Health::Dead(message) = health {
            let _ = stderr
                .write_all(format!("verve isolate died: {message}\n").as_bytes())
                .await;
            control.finish(HostExit::Died(message));
        }
        if control.ended() {
            let _ = stdout.shutdown().await;
            let _ = stderr.shutdown().await;
            return;
        }
    }
}
