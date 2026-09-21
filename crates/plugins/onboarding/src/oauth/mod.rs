//! OpenAI Codex OAuth login, independent of any frontend.
//!
//! Kept out of any binary so every front end drives the *same* login, and so
//! provider settings cannot edit a `config.json` entry without a way to obtain
//! the credential it points at.
//!
//! The phases stay free functions rather than a reducer: each frontend owns its
//! own event loop and renders its own sub-views, and they only agree on where
//! the boundaries are — prepare, collect (listen or paste), exchange+persist.
//! That also keeps every phase individually testable without a UI.
//!
//! ## Threading
//!
//! [`listener::wait_for_callback_with_cancel`] blocks its thread until the
//! browser comes back, the caller cancels, or the timeout expires, so callers
//! run it off their UI thread. [`flow::exchange_and_persist`] is `async`;
//! [`flow::exchange_and_persist_blocking`] wraps it in a private runtime for
//! callers that have no reactor of their own (the GPUI app).

pub mod flow;
pub mod listener;

pub use flow::{
    exchange_and_persist, exchange_and_persist_blocking, launch_browser, parse_pasted_callback,
    prepare_openai_oauth, FlowError, OAuthChallenge, CALLBACK_TIMEOUT, TOKEN_EXCHANGE_TIMEOUT,
};
pub use listener::{
    probe_port_free, wait_for_callback_with_cancel, CallbackResult, ListenerError, CALLBACK_PATH,
    CALLBACK_PORT,
};
