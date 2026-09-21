/// One connection, spoken as ACP: the loop between the probe and the gate.
pub mod acp_connection;
/// What a connection is allowed to do, and when: the `initialize` gate the
/// token hangs on. A state machine with no I/O, because every rule in it is
/// about ordering and authority.
pub mod acp_gate;
/// A pending permission as the ACP request that asks it. Its own module
/// because two protocols now ask the same question.
pub mod acp_permission;
mod acp_prompt;
/// The owner's half of a subscribed ACP connection: one writer, and the
/// translation from a session event to what goes out on the wire.
pub mod acp_stream;
pub mod commands;
pub mod events;
pub mod permissions;
pub mod questions;
pub mod server;
pub mod teammates;
/// Which protocol a connection opened with. Its own module because the
/// decision is made once per connection, before anything else on that
/// connection has a meaning.
pub mod wire_probe;

pub(crate) use commands::*;
pub(crate) use events::*;
pub(crate) use permissions::*;
pub(crate) use questions::*;
pub(crate) use server::*;
pub(crate) use teammates::*;
