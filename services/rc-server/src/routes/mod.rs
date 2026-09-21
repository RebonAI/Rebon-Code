//! Request handlers, one module per resource family.
//!
//! Every handler returns a bare [`axum::response::Response`] rather
//! than a typed result, so the uniform error body in
//! `normalize_response` (in the crate root) is the only thing a client
//! ever sees on failure. [`stream`] is the one WebSocket route; it
//! answers with the same statuses as the rest before it upgrades.
//! [`history`] is the paged read side of what [`stream`] persists.

pub mod devices;
pub mod environments;
pub mod health;
pub mod history;
pub mod sessions;
pub mod stream;
pub mod work;
