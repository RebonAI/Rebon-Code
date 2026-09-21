//! The subcommands of `sandbox-win.exe`.
//!
//! [`crate::core`] decides what should be true and [`crate::sys`] makes it so;
//! this layer is what a user (or Rebon) actually asks for on the command line.
//! Nothing here holds Win32 state of its own — each module reads an argv, calls
//! into the other two, and turns the answer into an exit code plus the stderr
//! markers.

pub mod cli;
pub mod exec;
pub mod provision;
pub mod reap;
pub mod status;
