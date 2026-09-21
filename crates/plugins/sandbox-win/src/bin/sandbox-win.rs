//! `sandbox-win.exe` — collect the argv and hand it to [`sandbox_win::run`].
//!
//! The name carries no `rebon-` prefix, unlike every other plugin binary: it is
//! the name the sandbox plugin installs and spawns, so the install contract
//! fixes it.

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(sandbox_win::run(&arguments));
}
