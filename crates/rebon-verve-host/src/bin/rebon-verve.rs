//! `rebon-verve`: a script as a process of its own, on Verve.
//!
//! What rebon starts where it used to start Node: a contained plugin host,
//! and a Code Mode program — each a separate process the OS sandbox wraps,
//! under Verve's permission model (`--permission --allow-fs-read=…`). It is
//! `verve run` and nothing else; see `verve::cli`.

fn main() -> std::process::ExitCode {
    verve::cli::main(std::env::args_os())
}
