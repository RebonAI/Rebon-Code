//! `rebon-verve`: a script as a process of its own, on Verve.
//!
//! What rebon starts where it used to start Node: a contained plugin host,
//! and a Code Mode program — each a separate process the OS sandbox wraps,
//! under Verve's permission model (`--permission --allow-fs-read=…`). It is
//! `verve run`, with the native Cordis defined as the in-process host
//! defines it, so a contained composition resolves `cordis` the same way; see
//! `verve::cli`.

fn main() -> std::process::ExitCode {
    verve::cli::main_with(std::env::args_os(), |runtime| {
        cordis_verve::install(runtime).map(drop)
    })
}
