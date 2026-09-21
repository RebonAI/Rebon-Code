//! `rebon-computer-use` — the Computer Use runtime, without a desktop app.
//!
//! Ships beside `rebon` and is started by hand in a terminal. It is a separate
//! executable rather than a subcommand because `serve` lends the process's main
//! thread to the platform and never starts a kernel or a session. `rebon
//! computer-use …` forwards here.
//!
//! The `ComputerUse` tool can reach a running runtime; this binary exists so
//! starting one does not require the desktop app. Without it, a TUI session, an
//! ACP session driven by an editor, or a background job has the tool listed and
//! refused at every call on any machine where the desktop app is not open.
//!
//! Nothing about the runtime needed a window. The backend runs off the UI
//! thread already, and the protocol acquires a target by naming a screen point
//! rather than by anyone clicking one — the app's window picker is a
//! convenience over `observe`, not the only way in. So this serves the same
//! runtime from the CLI and publishes the same endpoint record every consumer
//! already reads.
//!
//! What is deliberately *not* here is the app's target *picker* — click a
//! window to choose it. `observe --at` is how a target is acquired without one.
//! The highlight overlay is not in that category: it is part of the runtime,
//! and a runtime served from here draws it, which is why `serve` lends the
//! platform its main thread on macOS (see [`rebon_plugin_computer_use::main_thread`]).

use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rebon_plugin_computer_use::{
    clear_endpoint_record, endpoint_looks_live, load_endpoint_record, main_thread, Action, Client,
    NativeBackend, Point, Request, ServiceState, SessionEndpoint, StatusResponse,
};
use tracing_subscriber::EnvFilter;

/// Exit code for "this platform has no Computer Use backend".
///
/// The binary is built for every platform on purpose — packaging then needs no
/// per-platform table — so the platform check is a run-time answer rather than
/// a missing file, and it gets a code of its own so a caller can tell it apart
/// from a runtime that failed.
const UNSUPPORTED_PLATFORM_EXIT_CODE: u8 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "rebon-computer-use",
    version,
    about = "Run or inspect the Computer Use runtime for this machine.",
    long_about = "Run or inspect the Computer Use runtime for this machine.\n\n\
                  The runtime drives a desktop window you pick; the ComputerUse tool \
                  connects to whichever one is published. Serving it here is what makes \
                  the tool usable from a terminal session, an editor over ACP, or a \
                  background job — none of which can start the desktop app."
)]
struct Cli {
    #[command(subcommand)]
    command: ComputerUseCommand,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum ComputerUseCommand {
    /// Run the runtime in the foreground until interrupted.
    Serve {
        /// Replace a published endpoint record even if something answers on it.
        #[arg(long)]
        force: bool,
    },
    /// Show the published runtime, whether it answers, and what it has locked.
    Status,
    /// Lock onto the window under a screen point, or re-read the locked one.
    Observe {
        /// `X,Y` in screen coordinates. Omit to refresh the current target.
        #[arg(long, value_name = "X,Y")]
        at: Option<String>,
    },
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    if let Some(reason) = unsupported_platform() {
        eprintln!("{reason}");
        return ExitCode::from(UNSUPPORTED_PLATFORM_EXIT_CODE);
    }
    // `serve` on macOS parks this thread in the platform's run loop, so the
    // runtime is built here rather than under `#[tokio::main]`, which would
    // have already taken it.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rebon-computer-use: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli.command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rebon-computer-use: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// One line saying why nothing here can work, or `None` where it can.
///
/// The backend is a "not supported here" stub off macOS and Windows, so this
/// is the same fact the stub reports — asked once, before any subcommand
/// starts printing a runtime that will never exist.
fn unsupported_platform() -> Option<&'static str> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Some(
            "rebon-computer-use: Computer Use has no backend on this platform (macOS and Windows only).",
        )
    }
}

/// Logs go to stderr; stdout is this command's human-readable report.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
}

async fn run(command: ComputerUseCommand) -> Result<()> {
    match command {
        ComputerUseCommand::Serve { force } => serve(force).await,
        ComputerUseCommand::Status => status().await,
        ComputerUseCommand::Observe { at } => observe(at).await,
    }
}

/// Runs the runtime until interrupted.
///
/// Foreground on purpose. The runtime owns a private directory and an endpoint
/// record that have to be cleaned up, and a process the user can see is a
/// process the user can stop — this is a service that can drive their desktop.
async fn serve(force: bool) -> Result<()> {
    if let Some(existing) = load_endpoint_record() {
        if endpoint_looks_live(&existing.socket_path) && !force {
            bail!(
                "a Computer Use runtime is already published at {}\n\n\
                 Stop it first, or pass --force to replace the record with this one.",
                existing.socket_path.display()
            );
        }
    }

    let endpoint = SessionEndpoint::allocate().map_err(anyhow::Error::msg)?;
    // Before the backend is built: it reads the activation-marker path from the
    // environment when it is constructed, and writes the marker itself once a
    // target is locked.
    //
    // SAFETY: start-up, single-threaded, before any task that reads the
    // environment has been spawned.
    unsafe { endpoint.publish_environment() };
    if let Err(error) = endpoint.publish_record() {
        // Descendants of this process still find the endpoint through the
        // environment; only unrelated processes need the record.
        tracing::warn!(%error, "failed to publish the Computer Use endpoint record");
        eprintln!("warning: could not write the endpoint record: {error}");
    }

    let backend = NativeBackend::new().map_err(|error| {
        endpoint.discard();
        anyhow::anyhow!("{error}")
    })?;

    println!("Computer Use runtime listening.");
    println!("  endpoint  {}", endpoint.socket_path.display());
    println!("  marker    {}", endpoint.active_path.display());
    println!();
    println!("No target is locked yet. From another shell:");
    println!("  rebon-computer-use observe --at <X>,<Y>");
    println!();
    println!("Sessions on this machine can now use the ComputerUse tool. Ctrl+C to stop.");

    let served = if main_thread::required() {
        serve_lending_the_main_thread(&endpoint, backend).await
    } else {
        tokio::select! {
            result = rebon_plugin_computer_use::serve(&endpoint.socket_path, endpoint.token.clone(), backend) => {
                result.map_err(|error| anyhow::anyhow!("{error}"))
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for interrupts")?;
                println!("\nStopping.");
                Ok(())
            }
        }
    };

    // Whichever way it ended, nothing should be left claiming to be live.
    let _ = clear_endpoint_record();
    endpoint.discard();
    served
}

/// Serves with the main thread given over to the platform.
///
/// macOS needs it: the overlay marshals onto the main dispatch queue, and
/// `Overlay::new` does it *synchronously*, so a main thread parked in `block_on`
/// would not merely skip the highlight — the first `observe` would hang on a
/// queue nobody drains. Serving therefore moves to a worker and the main thread
/// runs the platform's loop, which is the same arrangement the desktop app has
/// always had.
async fn serve_lending_the_main_thread(
    endpoint: &SessionEndpoint,
    backend: NativeBackend,
) -> Result<()> {
    let (parker, stopper) = main_thread::pair();

    let socket = endpoint.socket_path.clone();
    let token = endpoint.token.clone();
    let on_end = stopper.clone();
    let mut served = tokio::spawn(async move {
        let outcome = rebon_plugin_computer_use::serve(&socket, token, backend).await;
        // The listener stopping on its own has to release the main thread too,
        // or the process would sit there serving nothing.
        on_end.stop();
        outcome
    });

    let on_interrupt = stopper.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_interrupt.stop();
        }
    });

    // Blocks this thread — the main one — until something stops it.
    parker.park();
    signal.abort();

    if served.is_finished() {
        return match (&mut served).await {
            Ok(outcome) => outcome.map_err(|error| anyhow::anyhow!("{error}")),
            Err(error) => Err(anyhow::anyhow!("the runtime task failed: {error}")),
        };
    }
    served.abort();
    println!("\nStopping.");
    Ok(())
}

async fn status() -> Result<()> {
    let Some(record) = load_endpoint_record() else {
        println!("No Computer Use runtime is published.");
        println!("  start one with: rebon-computer-use serve");
        return Ok(());
    };

    println!("endpoint  {}", record.socket_path.display());
    let live = endpoint_looks_live(&record.socket_path);
    println!(
        "listening {}",
        if live { "yes" } else { "no (stale record)" }
    );
    if !live {
        return Ok(());
    }
    println!(
        "target    {}",
        if record.active_path.exists() {
            "locked"
        } else {
            "none — run `rebon-computer-use observe --at <X>,<Y>`"
        }
    );

    let client = Client::new(record.socket_path, record.token);
    match client.request(Request::Status).await {
        Ok(response) => print_status(&response.status),
        Err(error) => println!("state     unreachable: {error}"),
    }
    Ok(())
}

/// Acquires a target, or refreshes the one already locked.
///
/// This is the whole of what the app's window picker does that the runtime
/// cares about: the protocol has no window-identifier field, so a screen point
/// is the only way to name a target, and `observe` with no point is how a
/// caller re-reads the target it already has.
async fn observe(at: Option<String>) -> Result<()> {
    let record = load_endpoint_record()
        .context("no Computer Use runtime is published; run `rebon-computer-use serve`")?;
    if !endpoint_looks_live(&record.socket_path) {
        bail!(
            "the published endpoint at {} is not answering; the runtime is not running",
            record.socket_path.display()
        );
    }
    let target = at.as_deref().map(parse_point).transpose()?;

    let client = Client::new(record.socket_path, record.token);
    let response = client
        .request(Request::Action {
            action: Action::Observe { target },
            target_epoch: None,
        })
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    print_status(&response.status);
    if let Some(shot) = response.screenshot {
        println!("screen    {}x{} @ {}x", shot.width, shot.height, shot.scale);
    }
    Ok(())
}

fn print_status(status: &StatusResponse) {
    println!(
        "state     {}",
        match status.state {
            ServiceState::WaitingForTarget => "waiting for a target",
            ServiceState::Active => "active",
            ServiceState::Paused => "paused",
            ServiceState::Stopped => "stopped",
            ServiceState::Unsupported => "unsupported on this platform",
        }
    );
    println!("epoch     {}", status.target_epoch);
    if let Some(window) = &status.target {
        println!(
            "window    {} (pid {}){}",
            window.owner_name,
            window.owner_pid,
            window
                .title
                .as_deref()
                .map(|title| format!(" — {title}"))
                .unwrap_or_default()
        );
        println!(
            "frame     {}x{} at {},{}",
            window.frame.width, window.frame.height, window.frame.x, window.frame.y
        );
    }
}

/// `X,Y` in screen coordinates. Separate from the tool's own parsing because
/// this one is typed by a person and says so when it is wrong.
fn parse_point(raw: &str) -> Result<Point> {
    let (x, y) = raw
        .split_once(',')
        .with_context(|| format!("expected `X,Y`, got {raw:?}"))?;
    let parse = |value: &str, axis: &str| -> Result<f64> {
        value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("{axis} in {raw:?} is not a number"))
    };
    Ok(Point {
        x: parse(x, "the X coordinate")?,
        y: parse(y, "the Y coordinate")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_is_two_numbers_separated_by_a_comma() {
        let point = parse_point("120, 340").unwrap();
        assert_eq!(point.x, 120.0);
        assert_eq!(point.y, 340.0);
    }

    #[test]
    fn a_malformed_point_says_what_was_expected() {
        let error = parse_point("120").unwrap_err().to_string();
        assert!(error.contains("X,Y"), "{error}");

        let error = parse_point("left,340").unwrap_err().to_string();
        assert!(error.contains("X coordinate"), "{error}");
    }
}
