use crate::{
    CallIdentity, TerminalStatus, WireContractError, WireEnvelope, WireMessage,
    MAX_SAFE_WIRE_INTEGER,
};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalCommit {
    pub status: TerminalStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalDisposition {
    Committed(TerminalCommit),
    Stale(StaleReason),
}

/// Whether a chunk may be delivered to whoever is waiting on its call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChunkDisposition {
    Accepted,
    /// Correctly formed but belonging to a past epoch or scope incarnation.
    Stale(StaleReason),
}

/// Cancel intent never owns a terminal slot. Repeated and late accepted intents
/// are idempotent; success, error, or cancelled terminal still wins one shared slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelDisposition {
    Accepted,
    Stale(StaleReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaleReason {
    HostEpoch { message: u64, current: u64 },
    ScopeGeneration { message: u64, current: u64 },
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum LifecycleError {
    #[error("call {call_id:?} is not registered")]
    UnknownCall { call_id: String },
    #[error("call {call_id:?} already committed terminal status {status:?}")]
    DuplicateTerminal {
        call_id: String,
        status: TerminalStatus,
    },
    #[error("message for call {call_id:?} is not terminal")]
    NotTerminal { call_id: String },
    #[error("message for call {call_id:?} is not a call/cancel notification with null payload")]
    NotCancel { call_id: String },
    #[error("message for call {call_id:?} is not a chunk")]
    NotChunk { call_id: String },
    #[error("call {call_id:?} produced a chunk after committing terminal status {status:?}")]
    ChunkAfterTerminal {
        call_id: String,
        status: TerminalStatus,
    },
    #[error("host epoch {actual} is newer than ledger epoch {expected}")]
    FutureHostEpoch { actual: u64, expected: u64 },
    #[error("scope {scope_id:?} for plugin {plugin_id:?} is not registered")]
    UnknownScope { plugin_id: String, scope_id: String },
    #[error("scope generation {actual} is newer than current generation {expected}")]
    FutureScopeGeneration { actual: u64, expected: u64 },
    #[error("scope generation regression for scope {scope_id:?} in plugin {plugin_id:?}: current {current}, attempted {attempted}")]
    ScopeGenerationRegression {
        plugin_id: String,
        scope_id: String,
        current: u64,
        attempted: u64,
    },
    #[error(
        "cannot advance scope generation beyond maximum safe wire integer {max}; current {current}"
    )]
    ScopeGenerationExhausted { current: u64, max: u64 },
    #[error("{field} value {value} exceeds maximum safe wire integer {max}")]
    UnsafeWireInteger {
        field: &'static str,
        value: u64,
        max: u64,
    },
    #[error("cannot register stale call identity: {reason:?}")]
    StaleCallIdentity { reason: StaleReason },
    #[error(
        "{field} identity mismatch for call {call_id:?}: expected {expected:?}, got {actual:?}"
    )]
    IdentityMismatch {
        call_id: String,
        field: &'static str,
        expected: String,
        actual: String,
    },
}

impl From<WireContractError> for LifecycleError {
    fn from(error: WireContractError) -> Self {
        match error {
            WireContractError::UnsafeInteger { field, value, max } => {
                Self::UnsafeWireInteger { field, value, max }
            }
        }
    }
}

/// Returns a strictly greater safe generation, or an explicit exhaustion error.
pub fn next_scope_generation(current: u64) -> Result<u64, LifecycleError> {
    validate_integer("scope_generation", current)?;
    current
        .checked_add(1)
        .filter(|next| *next <= MAX_SAFE_WIRE_INTEGER)
        .ok_or(LifecycleError::ScopeGenerationExhausted {
            current,
            max: MAX_SAFE_WIRE_INTEGER,
        })
}

#[derive(Clone, Debug)]
struct CallState {
    identity: CallIdentity,
    terminal: Option<TerminalStatus>,
}

/// Validator for authoritative scope generations and at most one committed
/// terminal per registered call. Generation advances are invalidation
/// linearization points; retained values also serve as closed-scope tombstones.
pub struct CallLedger {
    host_epoch: u64,
    scope_generations: HashMap<(String, String), u64>,
    calls: HashMap<String, CallState>,
}

impl CallLedger {
    /// Fallible because an unsafe epoch can never be represented on the wire.
    pub fn new(host_epoch: u64) -> Result<Self, LifecycleError> {
        validate_integer("host_epoch", host_epoch)?;
        Ok(Self {
            host_epoch,
            scope_generations: HashMap::new(),
            calls: HashMap::new(),
        })
    }

    /// Establishes or monotonically advances a scope's authoritative generation.
    /// Equal values are idempotent, jumps are allowed, and failures do not mutate.
    pub fn advance_scope_generation(
        &mut self,
        plugin_id: impl Into<String>,
        scope_id: impl Into<String>,
        generation: u64,
    ) -> Result<(), LifecycleError> {
        validate_integer("scope_generation", generation)?;
        let key = (plugin_id.into(), scope_id.into());
        if let Some(&current) = self.scope_generations.get(&key) {
            if generation < current {
                return Err(LifecycleError::ScopeGenerationRegression {
                    plugin_id: key.0,
                    scope_id: key.1,
                    current,
                    attempted: generation,
                });
            }
        }
        self.scope_generations.insert(key, generation);
        Ok(())
    }

    pub fn register_call(&mut self, identity: CallIdentity) -> Result<(), LifecycleError> {
        validate_identity(&identity)?;
        self.classify_lifetime(&identity)?;
        if self.calls.contains_key(&identity.call_id) {
            return Err(LifecycleError::IdentityMismatch {
                call_id: identity.call_id.clone(),
                field: "call_id",
                expected: "unique call_id".to_owned(),
                actual: identity.call_id,
            });
        }
        self.calls.insert(
            identity.call_id.clone(),
            CallState {
                identity,
                terminal: None,
            },
        );
        Ok(())
    }

    /// Validates cancel intent without registering a call or consuming/mutating
    /// its terminal slot. Duplicate and post-terminal cancel intent is accepted.
    pub fn submit_cancel(
        &self,
        envelope: &WireEnvelope,
    ) -> Result<CancelDisposition, LifecycleError> {
        validate_identity(&envelope.identity)?;
        if envelope.identity.host_epoch < self.host_epoch {
            return Ok(CancelDisposition::Stale(StaleReason::HostEpoch {
                message: envelope.identity.host_epoch,
                current: self.host_epoch,
            }));
        }
        if envelope.identity.host_epoch > self.host_epoch {
            return Err(LifecycleError::FutureHostEpoch {
                actual: envelope.identity.host_epoch,
                expected: self.host_epoch,
            });
        }
        if let Some(state) = self.calls.get(&envelope.identity.call_id) {
            check_identity(&state.identity, &envelope.identity)?;
        }
        if let Some(stale) = self.classify_stale(&envelope.identity)? {
            return Ok(CancelDisposition::Stale(stale));
        }
        let state = self.calls.get(&envelope.identity.call_id).ok_or_else(|| {
            LifecycleError::UnknownCall {
                call_id: envelope.identity.call_id.clone(),
            }
        })?;
        check_full_identity(&state.identity, &envelope.identity)?;
        if !envelope.is_cancel() {
            return Err(LifecycleError::NotCancel {
                call_id: envelope.identity.call_id.clone(),
            });
        }
        Ok(CancelDisposition::Accepted)
    }

    /// Validates one chunk of a call's answer.
    ///
    /// Chunks are checked, not recorded: they carry no state the ledger needs,
    /// and a stream of a million of them must not grow it. What the ledger does
    /// own is the boundary — **a chunk after the terminal is an error**, not a
    /// late arrival to be dropped. Unlike a duplicate terminal, which is a race
    /// two peers can legitimately lose, a chunk after the end means the producer
    /// kept emitting after saying it was done, and delivering it would hand a
    /// caller content for an answer it has already finished reading.
    pub fn submit_chunk(
        &self,
        envelope: &WireEnvelope,
    ) -> Result<ChunkDisposition, LifecycleError> {
        validate_identity(&envelope.identity)?;
        if !matches!(envelope.message, WireMessage::Chunk { .. }) {
            return Err(LifecycleError::NotChunk {
                call_id: envelope.identity.call_id.clone(),
            });
        }
        if let Some(state) = self.calls.get(&envelope.identity.call_id) {
            check_identity(&state.identity, &envelope.identity)?;
        }
        if let Some(stale) = self.classify_stale(&envelope.identity)? {
            return Ok(ChunkDisposition::Stale(stale));
        }
        let state = self.calls.get(&envelope.identity.call_id).ok_or_else(|| {
            LifecycleError::UnknownCall {
                call_id: envelope.identity.call_id.clone(),
            }
        })?;
        check_full_identity(&state.identity, &envelope.identity)?;
        if let Some(status) = state.terminal {
            return Err(LifecycleError::ChunkAfterTerminal {
                call_id: envelope.identity.call_id.clone(),
                status,
            });
        }
        Ok(ChunkDisposition::Accepted)
    }

    /// Success, error, and cancelled terminals compete for one slot; first wins.
    pub fn submit_terminal(
        &mut self,
        envelope: &WireEnvelope,
    ) -> Result<TerminalDisposition, LifecycleError> {
        validate_identity(&envelope.identity)?;
        if envelope.identity.host_epoch < self.host_epoch {
            return Ok(TerminalDisposition::Stale(StaleReason::HostEpoch {
                message: envelope.identity.host_epoch,
                current: self.host_epoch,
            }));
        }
        if envelope.identity.host_epoch > self.host_epoch {
            return Err(LifecycleError::FutureHostEpoch {
                actual: envelope.identity.host_epoch,
                expected: self.host_epoch,
            });
        }
        if let Some(state) = self.calls.get(&envelope.identity.call_id) {
            check_identity(&state.identity, &envelope.identity)?;
        }
        if let Some(stale) = self.classify_stale(&envelope.identity)? {
            return Ok(TerminalDisposition::Stale(stale));
        }

        let call_id = &envelope.identity.call_id;
        let state = self
            .calls
            .get_mut(call_id)
            .ok_or_else(|| LifecycleError::UnknownCall {
                call_id: call_id.clone(),
            })?;
        check_full_identity(&state.identity, &envelope.identity)?;
        let status = match envelope.message {
            WireMessage::Terminal { status, .. } => status,
            _ => {
                return Err(LifecycleError::NotTerminal {
                    call_id: call_id.clone(),
                })
            }
        };
        if let Some(committed) = state.terminal {
            return Err(LifecycleError::DuplicateTerminal {
                call_id: call_id.clone(),
                status: committed,
            });
        }
        state.terminal = Some(status);
        Ok(TerminalDisposition::Committed(TerminalCommit { status }))
    }

    /// Whether an identity still names a live epoch and scope incarnation.
    ///
    /// `None` means current. `Some(reason)` means the frame is correctly formed
    /// but belongs to an incarnation that has been invalidated — the contract
    /// says to discard it, not to treat it as an error. An unknown scope or a
    /// future epoch stays an error, because neither can be produced by a peer
    /// that is following the protocol.
    pub fn classify_identity(
        &self,
        identity: &CallIdentity,
    ) -> Result<Option<StaleReason>, LifecycleError> {
        validate_identity(identity)?;
        self.classify_stale(identity)
    }

    fn classify_lifetime(&self, identity: &CallIdentity) -> Result<(), LifecycleError> {
        match self.classify_stale(identity)? {
            Some(reason) => Err(LifecycleError::StaleCallIdentity { reason }),
            None => Ok(()),
        }
    }

    fn classify_stale(
        &self,
        identity: &CallIdentity,
    ) -> Result<Option<StaleReason>, LifecycleError> {
        if identity.host_epoch < self.host_epoch {
            return Ok(Some(StaleReason::HostEpoch {
                message: identity.host_epoch,
                current: self.host_epoch,
            }));
        }
        if identity.host_epoch > self.host_epoch {
            return Err(LifecycleError::FutureHostEpoch {
                actual: identity.host_epoch,
                expected: self.host_epoch,
            });
        }
        let key = (identity.plugin_id.clone(), identity.scope_id.clone());
        let current = self.scope_generations.get(&key).copied().ok_or_else(|| {
            LifecycleError::UnknownScope {
                plugin_id: identity.plugin_id.clone(),
                scope_id: identity.scope_id.clone(),
            }
        })?;
        if identity.scope_generation < current {
            Ok(Some(StaleReason::ScopeGeneration {
                message: identity.scope_generation,
                current,
            }))
        } else if identity.scope_generation > current {
            Err(LifecycleError::FutureScopeGeneration {
                actual: identity.scope_generation,
                expected: current,
            })
        } else {
            Ok(None)
        }
    }
}

fn validate_integer(field: &'static str, value: u64) -> Result<(), LifecycleError> {
    if value <= MAX_SAFE_WIRE_INTEGER {
        Ok(())
    } else {
        Err(LifecycleError::UnsafeWireInteger {
            field,
            value,
            max: MAX_SAFE_WIRE_INTEGER,
        })
    }
}

fn validate_identity(identity: &CallIdentity) -> Result<(), LifecycleError> {
    identity.validate_safe_integers().map_err(Into::into)
}

fn check_identity(expected: &CallIdentity, actual: &CallIdentity) -> Result<(), LifecycleError> {
    for (field, expected_value, actual_value) in [
        ("plugin_id", &expected.plugin_id, &actual.plugin_id),
        ("scope_id", &expected.scope_id, &actual.scope_id),
    ] {
        if expected_value != actual_value {
            return Err(LifecycleError::IdentityMismatch {
                call_id: actual.call_id.clone(),
                field,
                expected: expected_value.clone(),
                actual: actual_value.clone(),
            });
        }
    }
    Ok(())
}

fn check_full_identity(
    expected: &CallIdentity,
    actual: &CallIdentity,
) -> Result<(), LifecycleError> {
    check_identity(expected, actual)?;
    for (field, expected_value, actual_value) in [
        (
            "host_epoch",
            expected.host_epoch.to_string(),
            actual.host_epoch.to_string(),
        ),
        (
            "scope_generation",
            expected.scope_generation.to_string(),
            actual.scope_generation.to_string(),
        ),
        ("call_id", expected.call_id.clone(), actual.call_id.clone()),
    ] {
        if expected_value != actual_value {
            return Err(LifecycleError::IdentityMismatch {
                call_id: actual.call_id.clone(),
                field,
                expected: expected_value,
                actual: actual_value,
            });
        }
    }
    Ok(())
}
