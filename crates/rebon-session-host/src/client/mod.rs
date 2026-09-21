//! Reaching a session host from outside it.
//!
//! The pieces under `client/` are layers of one thing:
//!
//! - [`owner`] answers "who holds this session, and can I command them", and
//!   carries the request/response transport, the lease renewal and the event
//!   stream. It is the layer that knows about sockets.
//! - [`session_host_client`] is what an endpoint holds: a resolver plus a
//!   connection scoped to one endpoint generation, with the paired timeouts,
//!   the call-id allocator, the typed failures and the prompt ladder.
//! - [`stream_watermark`] is the bookkeeping every follower of a session needs
//!   to keep one delta from landing twice when the stream and the event log
//!   both carry it. It answers questions and holds no payload, so the
//!   projection it guards stays with the consumer.
//! - [`protocol_probe`] answers "which protocol does the owner at this
//!   endpoint speak", once per endpoint generation. It exists only for the
//!   compatibility release and goes with the legacy protocol.
//!
//! Everything here is re-exported flat from the crate root, so no path outside
//! this crate changed when the files moved.

/// One long connection to an owner that speaks ACP: one writer, one reader,
/// requests paired by id.
pub mod acp_link;
/// The owner's event stream read as ACP and handed back as `SessionEvent`,
/// plus the write half a permission answer goes out on.
pub mod acp_subscription;
pub mod owner;
pub mod protocol_probe;
pub mod session_host_client;
pub mod stream_watermark;
/// A host failure as a JSON-RPC error and back. Both halves read this one
/// table: the owner encodes, the client decodes.
pub mod wire_errors;
