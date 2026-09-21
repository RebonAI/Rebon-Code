//! Decision logic for `sandbox-win.exe`, the Windows sandbox helper.
//!
//! The helper assembles confinement out of four unrelated Windows mechanisms —
//! a downgraded user account for the security context, WFP filters keyed on that
//! account's SID for the network, deny ACEs for the filesystem, and a Job Object
//! for the process tree — and this module is everything about that which is a
//! *decision* rather than a *syscall*.
//!
//! ## Why the split exists
//!
//! The part most likely to be written wrong is the judgement — which ACE goes
//! first, whether a ledger entry may be revoked, what a filter is allowed to be
//! conditioned on — and the machine where that judgement is written is usually
//! not a machine that can run it. So none of it lives behind `cfg(windows)`;
//! `cargo test` runs the whole matrix on any host. [`crate::sys`] is the thin
//! shell that turns these values into Win32 calls, and [`crate::helper`] is what
//! wires them up.
//!
//! `unsafe` is forbidden here, which is the mechanical form of that promise.
//!
//! ## The load-bearing invariants
//!
//! Each of these is a place where being wrong looks exactly like working:
//!
//! * **Deny ACEs precede grants** ([`acl::is_correctly_ordered`]). Windows
//!   evaluates a DACL in order and stops at the first match, so a deny that
//!   sorts after a grant is decoration.
//! * **The sandbox never gets `WRITE_DAC`**, not even inside its own write root
//!   ([`acl::AceKind::access_mask`]). Otherwise it can delete the deny ACE that
//!   confines it.
//! * **Unknown flags are refused, never ignored** ([`argv::parse_exec`]). A
//!   helper that skips a flag it does not recognise reports success while
//!   enforcing less than it was asked to, which is also why [`status`] carries a
//!   version.
//! * **WFP filters are conditioned on the sandbox account's SID and nothing
//!   else** ([`wfp::plan_filters`]). The install prompt promises the user their
//!   own network is untouched; that promise is only true if no filter can match
//!   on an image path or a port.
//! * **A ledger entry is revoked by file identity, never by path**
//!   ([`ledger::revoke_decision`]). ACEs are a persistent modification of the
//!   user's real disk, and a renamed path would send the revoke to a different
//!   file.

#![forbid(unsafe_code)]

pub mod account;
pub mod acl;
pub mod argv;
pub mod cmdline;
pub mod desktop;
pub mod env;
pub mod ledger;
pub mod markers;
pub mod sddl;
pub mod status;
pub mod wfp;

pub use account::{account_for, SANDBOX_GROUP, SANDBOX_USER, SANDBOX_USER_NO_NETWORK};
pub use acl::{plan_aces, AceKind, AcePlan, PlannedAce};
pub use argv::{parse_exec, render_exec, ArgvError, ExecRequest, MaskRule};
pub use cmdline::{build_command_line, parse_command_line, quote_argument};
pub use env::EnvBlock;
pub use ledger::{Ledger, LedgerEntry, Observation, RevokeDecision};
pub use markers::{denied, failure, EXIT_HELPER_FAILURE};
pub use status::{StatusFacts, STATUS_VERSION};
