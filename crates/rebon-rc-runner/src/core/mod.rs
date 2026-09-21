//! The runner's decisions, with no I/O in them.
//!
//! Everything a work item can meet — an owner event, a controller frame, a
//! close code, a heartbeat answer, a malformed item — is turned into a
//! value here and acted on by the adapters (`crate::session`,
//! `crate::host`, `crate::transport`). Tested with plain values.

pub mod downlink;
pub mod ids;
pub mod limits;
pub mod owner;
pub mod state;
pub mod uplink;
pub mod work;
