//! Transport-independent wire contract for the plugin plane.
//!
//! `plugin_id` is identity metadata, not authentication: whoever supervises the
//! plugin plane must inject identities from trusted state rather than from argv or
//! environment, and admission must reserve and reject [`PLATFORM_PLUGIN_ID`] and
//! [`PLATFORM_CONTROL_SCOPE_ID`] so a manifest cannot impersonate control traffic.
//! This crate defines the wire shape, not authorization.
//!
//! Scope open/close generations belong to whoever holds authoritative scope state.
//! First open establishes the generation and repeating it is idempotent. To begin
//! close/reload, advance to a strictly greater generation before sending
//! `scope/close` carrying that new generation; the advance is the invalidation
//! linearization point. Keep the new generation as a tombstone after close and
//! reuse it on reopen; increment again only for the next close/reload. Jumps are
//! allowed, regressions are not. The ledger guarantees at most one committed
//! terminal per call and makes no terminal-delivery liveness guarantee.

mod codec;
mod lifecycle;
pub mod methods;
mod payload;
mod registry;

pub use codec::{CodecError, NdjsonCodec};
pub use lifecycle::{
    next_scope_generation, CallLedger, CancelDisposition, ChunkDisposition, LifecycleError,
    StaleReason, TerminalCommit, TerminalDisposition,
};
pub use methods::{
    CommandInvokeRequest, EventDelivery, EventEmitRequest, EventSubscribeRequest,
    EventUnsubscribeRequest, LlmControlRequest, LlmStreamRequest, PayloadError,
    PluginCommandCategory, PluginCommandDefinition, PluginCommandKind, PluginCommandSurface,
    PluginDrainReport, PluginLoadRequest, PluginReadyReport, PluginToolDefinition,
    PluginUnloadRequest, SeatCallRequest, ServiceCallRequest, ToolInvokeRequest,
    COMMAND_INVOKE_METHOD, EVENT_DELIVER_METHOD, EVENT_EMIT_METHOD, EVENT_SUBSCRIBE_METHOD,
    EVENT_UNSUBSCRIBE_METHOD, LLM_CONTROL_METHOD, LLM_CONTROL_SIGNALS, LLM_STREAM_METHOD,
    PLUGIN_LOAD_METHOD, PLUGIN_UNLOAD_METHOD, SEAT_CALL_METHOD, SERVICE_CALL_METHOD,
    TOOL_CALL_METHOD, TOOL_INVOKE_METHOD,
};
pub use payload::Payload;
pub use registry::{
    CallClosed, PluginPhase, PluginRegistry, RegistryError, Subscription, UNKNOWN_PLUGIN_CODE,
};

use serde::{
    de::{self, MapAccess, Visitor},
    ser::SerializeStruct,
    Deserialize, Deserializer, Serialize, Serializer,
};
use thiserror::Error;

/// Wire protocol version emitted and accepted by this crate.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest integer exactly representable by a JavaScript JSON number (2^53 - 1).
pub const MAX_SAFE_WIRE_INTEGER: u64 = 9_007_199_254_740_991;
/// Reserved platform plugin namespace. Plugin manifests must not use this value.
pub const PLATFORM_PLUGIN_ID: &str = "$rebon/platform";
/// Reserved platform control scope. Plugin manifests must not use this value.
pub const PLATFORM_CONTROL_SCOPE_ID: &str = "$rebon/control";
/// Fixed generation for supervisor-created platform control identities.
pub const PLATFORM_CONTROL_GENERATION: u64 = 0;
/// Host-level method reserved for platform-control identity. Enforcing that
/// identity belongs to the host; the payload is undefined here.
pub const PLATFORM_INITIALIZE_METHOD: &str = "platform/initialize";
/// Host-level method reserved for platform-control identity. Enforcing that
/// identity belongs to the host; the payload is undefined here.
pub const PLATFORM_SHUTDOWN_METHOD: &str = "platform/shutdown";
/// Notification method whose payload is null and identity is the target call.
pub const CALL_CANCEL_METHOD: &str = "call/cancel";
/// Scope lifecycle method; this crate does not define its payload schema.
pub const SCOPE_OPEN_METHOD: &str = "scope/open";
/// Scope lifecycle method carrying the already-advanced generation; this crate
/// does not define its payload schema.
pub const SCOPE_CLOSE_METHOD: &str = "scope/close";

/// Default maximum JSON frame body size: 8 MiB (8 * 1024 * 1024 bytes).
/// The trailing LF and optional CR are not counted.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WireContractError {
    #[error("{field} value {value} exceeds maximum safe wire integer {max}")]
    UnsafeInteger {
        field: &'static str,
        value: u64,
        max: u64,
    },
}

/// Identity shared by every message participating in one call.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallIdentity {
    #[serde(with = "host_epoch_integer")]
    pub host_epoch: u64,
    pub plugin_id: String,
    pub scope_id: String,
    #[serde(with = "scope_generation_integer")]
    pub scope_generation: u64,
    pub call_id: String,
}

impl CallIdentity {
    /// Constructs the reserved identity that only platform-control traffic may
    /// carry. Plugin-supplied manifests must never reach this value.
    pub fn platform_control(
        host_epoch: u64,
        call_id: impl Into<String>,
    ) -> Result<Self, WireContractError> {
        validate_safe_integer("host_epoch", host_epoch)?;
        Ok(Self {
            host_epoch,
            plugin_id: PLATFORM_PLUGIN_ID.into(),
            scope_id: PLATFORM_CONTROL_SCOPE_ID.into(),
            scope_generation: PLATFORM_CONTROL_GENERATION,
            call_id: call_id.into(),
        })
    }

    pub fn is_platform_control(&self) -> bool {
        self.plugin_id == PLATFORM_PLUGIN_ID
            && self.scope_id == PLATFORM_CONTROL_SCOPE_ID
            && self.scope_generation == PLATFORM_CONTROL_GENERATION
    }

    pub(crate) fn validate_safe_integers(&self) -> Result<(), WireContractError> {
        validate_safe_integer("host_epoch", self.host_epoch)?;
        validate_safe_integer("scope_generation", self.scope_generation)
    }
}

fn validate_safe_integer(field: &'static str, value: u64) -> Result<(), WireContractError> {
    if value <= MAX_SAFE_WIRE_INTEGER {
        Ok(())
    } else {
        Err(WireContractError::UnsafeInteger {
            field,
            value,
            max: MAX_SAFE_WIRE_INTEGER,
        })
    }
}

macro_rules! safe_integer_serde {
    ($module:ident, $field:literal) => {
        mod $module {
            use super::validate_safe_integer;
            use serde::{Deserialize, Deserializer, Serializer};
            use serde_json::value::RawValue;

            pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                validate_safe_integer($field, *value).map_err(serde::ser::Error::custom)?;
                serializer.serialize_u64(*value)
            }

            /// Reads the number's **exact decimal token** rather than a parsed
            /// value, because the question this answers — "is this an integer
            /// this protocol can carry?" — is about what was written, not about
            /// the nearest `f64` to it.
            pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
            where
                D: Deserializer<'de>,
            {
                let raw = Box::<RawValue>::deserialize(deserializer)?;
                super::parse_wire_integer($field, raw.get()).map_err(serde::de::Error::custom)
            }
        }
    };
}

safe_integer_serde!(host_epoch_integer, "host_epoch");
safe_integer_serde!(scope_generation_integer, "scope_generation");

/// Validates one identity integer from the exact token it was written as.
///
/// Two distinct refusals, and the difference matters to a caller: a token that
/// is not a JSON number at all is the wrong shape, while a number outside the
/// carryable range is a value problem.
pub(crate) fn parse_wire_integer(field: &'static str, token: &str) -> Result<u64, String> {
    let token = token.trim();
    if !token.starts_with(|c: char| c == '-' || c.is_ascii_digit()) {
        return Err(format!(
            "invalid type: {field} must be a JSON number, got `{token}`"
        ));
    }
    parse_safe_integer(token).ok_or_else(|| {
        format!("{field} value {token} is not an integer from 0 through {MAX_SAFE_WIRE_INTEGER}")
    })
}

/// Whether a decimal token denotes an integer in `0..=MAX_SAFE_WIRE_INTEGER`,
/// and which one. Exponents and trailing zero fractions are honoured; anything
/// that would need rounding to become an integer is refused.
fn parse_safe_integer(text: &str) -> Option<u64> {
    let (negative, unsigned) = text
        .strip_prefix('-')
        .map_or((false, text), |unsigned| (true, unsigned));
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, "0"), |parts| parts);
    let exponent_digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    if exponent_digits.is_empty() || !exponent_digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) if !whole.is_empty() && !fraction.is_empty() => (whole, fraction),
        None if !mantissa.is_empty() => (mantissa, ""),
        _ => return None,
    };
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    if whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte == b'0')
    {
        return Some(0);
    }
    if negative {
        return None;
    }
    let exponent: i64 = exponent.parse().ok()?;
    let digits = format!("{whole}{fraction}");
    let scale = exponent.checked_sub(i64::try_from(fraction.len()).ok()?)?;

    let integer_digits = if scale >= 0 {
        let zero_count = usize::try_from(scale).ok()?;
        if digits
            .trim_start_matches('0')
            .len()
            .saturating_add(zero_count)
            > 16
        {
            return None;
        }
        format!("{digits}{}", "0".repeat(zero_count))
    } else {
        let removed = usize::try_from(scale.unsigned_abs()).ok()?;
        if removed > digits.len()
            || !digits[digits.len() - removed..]
                .bytes()
                .all(|byte| byte == b'0')
        {
            return None;
        }
        digits[..digits.len() - removed].to_owned()
    };
    let significant = integer_digits.trim_start_matches('0');
    if significant.is_empty() {
        return Some(0);
    }
    if significant.len() > 16 {
        return None;
    }
    let value = significant.parse().ok()?;
    (value <= MAX_SAFE_WIRE_INTEGER).then_some(value)
}

/// Versioned plugin-plane wire envelope.
///
/// `Serialize`/`Deserialize` are written out rather than derived. The derive
/// would need `#[serde(flatten)]` for the identity and `#[serde(tag = "type")]`
/// for the message, and both route every field through serde's `Content`
/// buffer — which turns numbers into parsed values and so destroys the exact
/// payload tokens this protocol is required to carry (see [`Payload`]). Writing
/// the visitors also puts the strictness in one readable place: field order on
/// the way out, and duplicate/unknown/missing rejection on the way in.
#[derive(Clone, Debug, PartialEq)]
pub struct WireEnvelope {
    pub protocol_version: u32,
    pub identity: CallIdentity,
    pub message: WireMessage,
}

const ENVELOPE_FIELDS: &[&str] = &[
    "protocol_version",
    "host_epoch",
    "plugin_id",
    "scope_id",
    "scope_generation",
    "call_id",
    "message",
];

impl Serialize for WireEnvelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(serde::ser::Error::custom(format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                self.protocol_version
            )));
        }
        self.identity
            .validate_safe_integers()
            .map_err(serde::ser::Error::custom)?;
        let mut envelope = serializer.serialize_struct("WireEnvelope", ENVELOPE_FIELDS.len())?;
        envelope.serialize_field("protocol_version", &self.protocol_version)?;
        envelope.serialize_field("host_epoch", &self.identity.host_epoch)?;
        envelope.serialize_field("plugin_id", &self.identity.plugin_id)?;
        envelope.serialize_field("scope_id", &self.identity.scope_id)?;
        envelope.serialize_field("scope_generation", &self.identity.scope_generation)?;
        envelope.serialize_field("call_id", &self.identity.call_id)?;
        envelope.serialize_field("message", &self.message)?;
        envelope.end()
    }
}

enum EnvelopeField {
    ProtocolVersion,
    HostEpoch,
    PluginId,
    ScopeId,
    ScopeGeneration,
    CallId,
    Message,
}

impl<'de> Deserialize<'de> for EnvelopeField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = EnvelopeField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a protocol envelope field name")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<EnvelopeField, E> {
                match value {
                    "protocol_version" => Ok(EnvelopeField::ProtocolVersion),
                    "host_epoch" => Ok(EnvelopeField::HostEpoch),
                    "plugin_id" => Ok(EnvelopeField::PluginId),
                    "scope_id" => Ok(EnvelopeField::ScopeId),
                    "scope_generation" => Ok(EnvelopeField::ScopeGeneration),
                    "call_id" => Ok(EnvelopeField::CallId),
                    "message" => Ok(EnvelopeField::Message),
                    other => Err(E::unknown_field(other, ENVELOPE_FIELDS)),
                }
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

/// Takes a field's value once, refusing a second occurrence of the same name.
macro_rules! take_once {
    ($slot:ident, $map:ident, $name:literal) => {{
        if $slot.is_some() {
            return Err(de::Error::duplicate_field($name));
        }
        $slot = Some($map.next_value()?);
    }};
}

impl<'de> Deserialize<'de> for WireEnvelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EnvelopeVisitor;
        impl<'de> Visitor<'de> for EnvelopeVisitor {
            type Value = WireEnvelope;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a protocol envelope object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WireEnvelope, A::Error> {
                let mut protocol_version: Option<u32> = None;
                let mut host_epoch: Option<u64> = None;
                let mut plugin_id: Option<String> = None;
                let mut scope_id: Option<String> = None;
                let mut scope_generation: Option<u64> = None;
                let mut call_id: Option<String> = None;
                let mut message: Option<WireMessage> = None;

                while let Some(field) = map.next_key::<EnvelopeField>()? {
                    match field {
                        EnvelopeField::ProtocolVersion => {
                            if protocol_version.is_some() {
                                return Err(de::Error::duplicate_field("protocol_version"));
                            }
                            let value: u32 = map.next_value()?;
                            if value != PROTOCOL_VERSION {
                                return Err(de::Error::custom(format!(
                                    "unsupported protocol version {value}; expected {PROTOCOL_VERSION}"
                                )));
                            }
                            protocol_version = Some(value);
                        }
                        EnvelopeField::HostEpoch => {
                            if host_epoch.is_some() {
                                return Err(de::Error::duplicate_field("host_epoch"));
                            }
                            host_epoch = Some(read_wire_integer(&mut map, "host_epoch")?);
                        }
                        EnvelopeField::PluginId => take_once!(plugin_id, map, "plugin_id"),
                        EnvelopeField::ScopeId => take_once!(scope_id, map, "scope_id"),
                        EnvelopeField::ScopeGeneration => {
                            if scope_generation.is_some() {
                                return Err(de::Error::duplicate_field("scope_generation"));
                            }
                            scope_generation =
                                Some(read_wire_integer(&mut map, "scope_generation")?);
                        }
                        EnvelopeField::CallId => take_once!(call_id, map, "call_id"),
                        EnvelopeField::Message => take_once!(message, map, "message"),
                    }
                }

                Ok(WireEnvelope {
                    protocol_version: protocol_version
                        .ok_or_else(|| de::Error::missing_field("protocol_version"))?,
                    identity: CallIdentity {
                        host_epoch: host_epoch
                            .ok_or_else(|| de::Error::missing_field("host_epoch"))?,
                        plugin_id: plugin_id
                            .ok_or_else(|| de::Error::missing_field("plugin_id"))?,
                        scope_id: scope_id.ok_or_else(|| de::Error::missing_field("scope_id"))?,
                        scope_generation: scope_generation
                            .ok_or_else(|| de::Error::missing_field("scope_generation"))?,
                        call_id: call_id.ok_or_else(|| de::Error::missing_field("call_id"))?,
                    },
                    message: message.ok_or_else(|| de::Error::missing_field("message"))?,
                })
            }
        }
        deserializer.deserialize_struct("WireEnvelope", ENVELOPE_FIELDS, EnvelopeVisitor)
    }
}

/// Reads an identity integer from its exact token rather than a parsed number.
fn read_wire_integer<'de, A: MapAccess<'de>>(
    map: &mut A,
    field: &'static str,
) -> Result<u64, A::Error> {
    let raw = map.next_value::<Box<serde_json::value::RawValue>>()?;
    parse_wire_integer(field, raw.get()).map_err(de::Error::custom)
}

impl WireEnvelope {
    pub fn new(identity: CallIdentity, message: WireMessage) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            identity,
            message,
        }
    }

    /// Builds the only valid cancel intent shape: a notification with the target
    /// call's complete identity and JSON null payload. It is not a new call.
    pub fn cancel(target: CallIdentity) -> Self {
        Self::new(
            target,
            WireMessage::Notification {
                method: CALL_CANCEL_METHOD.into(),
                payload: Payload::null(),
            },
        )
    }

    pub fn is_cancel(&self) -> bool {
        matches!(
            &self.message,
            WireMessage::Notification { method, payload }
                if method == CALL_CANCEL_METHOD && payload.is_null()
        )
    }
}

/// Minimal message vocabulary. Method names and payloads stay opaque: this layer
/// carries them without interpreting them, and it does not reject unknown methods.
#[derive(Clone, Debug, PartialEq)]
pub enum WireMessage {
    Request {
        method: String,
        payload: Payload,
    },
    Terminal {
        status: TerminalStatus,
        payload: Payload,
    },
    Notification {
        method: String,
        payload: Payload,
    },
    /// One piece of a call's answer, before the answer has ended.
    ///
    /// A chunk carries no method: it belongs to the call whose identity it
    /// rides, and that call's method already said what it means. It never
    /// competes for the terminal slot — a call may carry any number of chunks
    /// and still exactly one terminal.
    Chunk {
        payload: Payload,
    },
}

/// Every name a message object may carry. The variant decides which subset is
/// legal, but a name outside this set is unknown whatever the variant is.
const MESSAGE_FIELDS: &[&str] = &["type", "method", "status", "payload"];
const ADDRESSED_FIELDS: &[&str] = &["type", "method", "payload"];
const TERMINAL_FIELDS: &[&str] = &["type", "status", "payload"];
const CHUNK_FIELDS: &[&str] = &["type", "payload"];
const MESSAGE_TYPES: &[&str] = &["request", "terminal", "notification", "chunk"];

impl Serialize for WireMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut message = serializer.serialize_struct("WireMessage", 3)?;
        match self {
            Self::Request { method, payload } => {
                message.serialize_field("type", "request")?;
                message.serialize_field("method", method)?;
                message.serialize_field("payload", payload)?;
            }
            Self::Terminal { status, payload } => {
                message.serialize_field("type", "terminal")?;
                message.serialize_field("status", status)?;
                message.serialize_field("payload", payload)?;
            }
            Self::Notification { method, payload } => {
                message.serialize_field("type", "notification")?;
                message.serialize_field("method", method)?;
                message.serialize_field("payload", payload)?;
            }
            Self::Chunk { payload } => {
                message.serialize_field("type", "chunk")?;
                message.serialize_field("payload", payload)?;
            }
        }
        message.end()
    }
}

enum MessageField {
    Type,
    Method,
    Status,
    Payload,
}

impl<'de> Deserialize<'de> for MessageField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = MessageField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a protocol message field name")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<MessageField, E> {
                match value {
                    "type" => Ok(MessageField::Type),
                    "method" => Ok(MessageField::Method),
                    "status" => Ok(MessageField::Status),
                    "payload" => Ok(MessageField::Payload),
                    other => Err(E::unknown_field(other, MESSAGE_FIELDS)),
                }
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for WireMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MessageVisitor;
        impl<'de> Visitor<'de> for MessageVisitor {
            type Value = WireMessage;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a protocol message object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WireMessage, A::Error> {
                let mut kind: Option<String> = None;
                let mut method: Option<String> = None;
                let mut status: Option<TerminalStatus> = None;
                let mut payload: Option<Payload> = None;

                // Fields are collected before the variant is decided, because a
                // message may name its `type` last and the payload still has to
                // be captured verbatim when it does.
                while let Some(field) = map.next_key::<MessageField>()? {
                    match field {
                        MessageField::Type => take_once!(kind, map, "type"),
                        MessageField::Method => take_once!(method, map, "method"),
                        MessageField::Status => take_once!(status, map, "status"),
                        MessageField::Payload => take_once!(payload, map, "payload"),
                    }
                }

                let kind = kind.ok_or_else(|| de::Error::missing_field("type"))?;
                let payload = payload.ok_or_else(|| de::Error::missing_field("payload"))?;
                match kind.as_str() {
                    "request" | "notification" => {
                        // A field that belongs to another variant is unknown
                        // *here*, which is what a sender needs to be told.
                        if status.is_some() {
                            return Err(de::Error::unknown_field("status", ADDRESSED_FIELDS));
                        }
                        let method = method.ok_or_else(|| de::Error::missing_field("method"))?;
                        Ok(if kind == "request" {
                            WireMessage::Request { method, payload }
                        } else {
                            WireMessage::Notification { method, payload }
                        })
                    }
                    "terminal" => {
                        if method.is_some() {
                            return Err(de::Error::unknown_field("method", TERMINAL_FIELDS));
                        }
                        Ok(WireMessage::Terminal {
                            status: status.ok_or_else(|| de::Error::missing_field("status"))?,
                            payload,
                        })
                    }
                    "chunk" => {
                        if method.is_some() {
                            return Err(de::Error::unknown_field("method", CHUNK_FIELDS));
                        }
                        if status.is_some() {
                            return Err(de::Error::unknown_field("status", CHUNK_FIELDS));
                        }
                        Ok(WireMessage::Chunk { payload })
                    }
                    other => Err(de::Error::unknown_variant(other, MESSAGE_TYPES)),
                }
            }
        }
        deserializer.deserialize_struct("WireMessage", MESSAGE_FIELDS, MessageVisitor)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Success,
    Error,
    Cancelled,
}
