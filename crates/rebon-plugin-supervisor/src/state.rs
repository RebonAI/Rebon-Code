//! Everything the supervisor knows, with no process attached.
//!
//! Splitting this out is not tidiness. The interesting behaviour of a
//! supervisor is what it does when the thing it supervises stops answering, and
//! that is exactly the behaviour that is miserable to test through a real
//! process. Here it is a function call.
//!
//! # The rule this exists to enforce
//!
//! **No caller waits forever.** Every call that was in flight when the host died
//! gets a terminal — a synthesised error naming the reason — before anything
//! else happens. A supervisor that merely notices the death and drops its
//! bookkeeping leaves every awaiting turn hanging, which is worse than a crash
//! because it has no message.
//!
//! # Epochs
//!
//! One state belongs to one host process. A restart makes a *new* epoch, and
//! frames from the old one are then stale by construction — the
//! [`CallLedger`] discards them rather than misrouting them into calls that
//! happen to reuse an id.

use std::collections::BTreeMap;

use rebon_plugin_protocol::{
    next_scope_generation, CallIdentity, CallLedger, CancelDisposition, ChunkDisposition,
    LifecycleError, Payload, PluginRegistry, TerminalDisposition, TerminalStatus, WireEnvelope,
    WireMessage, PLATFORM_CONTROL_GENERATION, PLATFORM_CONTROL_SCOPE_ID, PLATFORM_PLUGIN_ID,
};
use thiserror::Error;

/// Why a host stopped being usable. Carried into every synthesised terminal so
/// a caller learns the cause instead of a bare "failed".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostFailure {
    pub reason: String,
    /// Tail of the host's stderr, when there was any. A crash without a
    /// diagnosis is the failure mode this field exists to prevent.
    pub diagnostics: Option<String>,
}

impl HostFailure {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            diagnostics: None,
        }
    }

    pub fn with_diagnostics(mut self, diagnostics: impl Into<String>) -> Self {
        let diagnostics = diagnostics.into();
        self.diagnostics = (!diagnostics.trim().is_empty()).then_some(diagnostics);
        self
    }

    fn payload(&self) -> Payload {
        let mut object = serde_json::Map::new();
        object.insert("code".into(), serde_json::Value::from(HOST_FAILED_CODE));
        object.insert(
            "message".into(),
            serde_json::Value::from(self.reason.clone()),
        );
        if let Some(diagnostics) = &self.diagnostics {
            object.insert(
                "diagnostics".into(),
                serde_json::Value::from(diagnostics.clone()),
            );
        }
        Payload::from(serde_json::Value::Object(object))
    }
}

impl std::fmt::Display for HostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.diagnostics {
            Some(diagnostics) => write!(f, "{}: {diagnostics}", self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

/// The terminal payload code every synthesised failure carries.
pub const HOST_FAILED_CODE: &str = "[PLUGIN_HOST_FAILED]";

#[derive(Debug, Error)]
pub enum StateError {
    #[error("plugin host is not usable: {0}")]
    HostFailed(HostFailure),
    #[error("{0}")]
    Lifecycle(#[from] LifecycleError),
    #[error("scope {scope_id:?} for plugin {plugin_id:?} was never opened")]
    UnknownScope { plugin_id: String, scope_id: String },
    #[error("scope {scope_id:?} for plugin {plugin_id:?} is already closed")]
    ScopeAlreadyClosed { plugin_id: String, scope_id: String },
}

/// What an inbound frame turned out to be.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    /// A terminal for a call this supervisor is waiting on.
    Terminal {
        call_id: String,
        status: TerminalStatus,
        payload: Payload,
    },
    /// Correctly formed but belonging to a past epoch or scope incarnation.
    /// Dropping it is the contract, not a failure.
    Stale { call_id: String },
    /// A request the host issued. Registered on this side's ledger, so exactly
    /// one answer is owed for it and a second one is refused rather than sent.
    Request {
        identity: CallIdentity,
        method: String,
        payload: Payload,
    },
    /// A notification the host issued. Nothing is owed for it, and nothing may
    /// be written back on its id.
    Notification {
        identity: CallIdentity,
        method: String,
        payload: Payload,
    },
    /// One piece of an answer that has not ended yet.
    Chunk { call_id: String, payload: Payload },
}

/// One open scope's authoritative generation.
#[derive(Clone, Copy, Debug)]
struct ScopeState {
    generation: u64,
    open: bool,
}

pub struct SupervisorState {
    host_epoch: u64,
    ledger: CallLedger,
    registry: PluginRegistry,
    /// Calls sent and not yet terminated, in send order.
    pending: BTreeMap<String, CallIdentity>,
    /// Requests the host issued that this side has not answered yet.
    ///
    /// Kept apart from `pending` because the two are owed in opposite
    /// directions: a host failure answers everything in `pending` and answers
    /// nothing here, since there is no longer anyone to answer to.
    owed: BTreeMap<String, CallIdentity>,
    scopes: BTreeMap<(String, String), ScopeState>,
    failure: Option<HostFailure>,
    next_call: u64,
}

impl SupervisorState {
    pub fn new(host_epoch: u64) -> Result<Self, StateError> {
        let mut ledger = CallLedger::new(host_epoch)?;
        register_control_scope(&mut ledger)?;
        Ok(Self {
            host_epoch,
            ledger,
            registry: PluginRegistry::new(),
            pending: BTreeMap::new(),
            owed: BTreeMap::new(),
            scopes: BTreeMap::new(),
            failure: None,
            next_call: 0,
        })
    }

    pub fn host_epoch(&self) -> u64 {
        self.host_epoch
    }

    pub fn failure(&self) -> Option<&HostFailure> {
        self.failure.as_ref()
    }

    pub fn is_alive(&self) -> bool {
        self.failure.is_none()
    }

    pub fn pending_calls(&self) -> Vec<String> {
        self.pending.keys().cloned().collect()
    }

    /// Requests from the host that this side still owes an answer for.
    pub fn owed_answers(&self) -> Vec<String> {
        self.owed.keys().cloned().collect()
    }

    pub fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut PluginRegistry {
        &mut self.registry
    }

    /// Ids are per-epoch and monotonic, so a late frame from a previous host can
    /// never collide with a live call even before the epoch check runs.
    pub fn next_call_id(&mut self) -> String {
        self.next_call += 1;
        format!("c{}-{}", self.host_epoch, self.next_call)
    }

    fn ensure_alive(&self) -> Result<(), StateError> {
        match &self.failure {
            Some(failure) => Err(StateError::HostFailed(failure.clone())),
            None => Ok(()),
        }
    }

    /// Reserves the scope's first generation, or advances it for a reopen.
    pub fn open_scope(
        &mut self,
        plugin_id: &str,
        scope_id: &str,
    ) -> Result<CallIdentity, StateError> {
        self.ensure_alive()?;
        let key = (plugin_id.to_owned(), scope_id.to_owned());
        let generation = match self.scopes.get(&key) {
            // A closed scope keeps its generation as a tombstone; reopening
            // reuses it rather than advancing, so the number only ever moves at
            // an invalidation.
            Some(state) => state.generation,
            None => 0,
        };
        self.ledger
            .advance_scope_generation(plugin_id, scope_id, generation)?;
        self.scopes.insert(
            key,
            ScopeState {
                generation,
                open: true,
            },
        );
        Ok(CallIdentity {
            host_epoch: self.host_epoch,
            plugin_id: plugin_id.to_owned(),
            scope_id: scope_id.to_owned(),
            scope_generation: generation,
            call_id: self.next_call_id(),
        })
    }

    /// Advances to the generation a close must carry. The advance *is* the
    /// invalidation: everything bound to the old generation, subscriptions
    /// included, stops being valid at this point rather than when the close is
    /// acknowledged.
    pub fn close_scope(
        &mut self,
        plugin_id: &str,
        scope_id: &str,
    ) -> Result<(CallIdentity, Vec<(String, String)>), StateError> {
        self.ensure_alive()?;
        let key = (plugin_id.to_owned(), scope_id.to_owned());
        let current = self
            .scopes
            .get(&key)
            .copied()
            .ok_or_else(|| StateError::UnknownScope {
                plugin_id: plugin_id.to_owned(),
                scope_id: scope_id.to_owned(),
            })?;
        // Closing twice would burn a generation for nothing, and generations are
        // the invalidation clock every subscription is pinned to.
        if !current.open {
            return Err(StateError::ScopeAlreadyClosed {
                plugin_id: plugin_id.to_owned(),
                scope_id: scope_id.to_owned(),
            });
        }
        let generation = next_scope_generation(current.generation)?;
        self.ledger
            .advance_scope_generation(plugin_id, scope_id, generation)?;
        self.scopes.insert(
            key,
            ScopeState {
                generation,
                open: false,
            },
        );
        let revoked = self
            .registry
            .revoke_stale_subscriptions(plugin_id, scope_id, generation);
        Ok((
            CallIdentity {
                host_epoch: self.host_epoch,
                plugin_id: plugin_id.to_owned(),
                scope_id: scope_id.to_owned(),
                scope_generation: generation,
                call_id: self.next_call_id(),
            },
            revoked,
        ))
    }

    /// An identity for a call into an open scope, on that scope's current
    /// generation. A closed scope has none: work addressed to it belongs to an
    /// incarnation that no longer exists.
    pub fn call_identity(
        &mut self,
        plugin_id: &str,
        scope_id: &str,
    ) -> Result<CallIdentity, StateError> {
        self.ensure_alive()?;
        let key = (plugin_id.to_owned(), scope_id.to_owned());
        let scope = self
            .scopes
            .get(&key)
            .copied()
            .ok_or_else(|| StateError::UnknownScope {
                plugin_id: plugin_id.to_owned(),
                scope_id: scope_id.to_owned(),
            })?;
        if !scope.open {
            return Err(StateError::ScopeAlreadyClosed {
                plugin_id: plugin_id.to_owned(),
                scope_id: scope_id.to_owned(),
            });
        }
        Ok(CallIdentity {
            host_epoch: self.host_epoch,
            plugin_id: plugin_id.to_owned(),
            scope_id: scope_id.to_owned(),
            scope_generation: scope.generation,
            call_id: self.next_call_id(),
        })
    }

    /// Registers a call about to be written. Called *before* the write, so a
    /// write that fails still leaves a call the fail-all can answer.
    pub fn begin_call(&mut self, identity: CallIdentity) -> Result<(), StateError> {
        self.ensure_alive()?;
        self.ledger.register_call(identity.clone())?;
        self.pending.insert(identity.call_id.clone(), identity);
        Ok(())
    }

    /// The identity of a call this side sent and is still waiting on.
    ///
    /// Cancelling needs the whole five-tuple, not just the id: the host's
    /// ledger checks every field, and an identity assembled from guesses would
    /// be refused rather than cancelling anything.
    pub fn pending_identity(&self, call_id: &str) -> Option<&CallIdentity> {
        self.pending.get(call_id)
    }

    /// Validates a cancel this side is about to send.
    pub fn admit_cancel(&self, envelope: &WireEnvelope) -> Result<bool, StateError> {
        Ok(matches!(
            self.ledger.submit_cancel(envelope)?,
            CancelDisposition::Accepted
        ))
    }

    /// Records this side's answer to a request the host issued.
    ///
    /// The answer goes through the same ledger, so "one answer per request" is
    /// enforced rather than intended: a second attempt is refused here instead
    /// of reaching the host as a duplicate terminal.
    ///
    /// Returns whether the answer should still be written. A scope whose
    /// generation advanced while the request was being handled has taken the
    /// asking incarnation with it, and the host discards terminals on a stale
    /// identity — writing one would only add a frame nobody reads.
    pub fn commit_inbound_terminal(&mut self, envelope: &WireEnvelope) -> Result<bool, StateError> {
        self.owed.remove(&envelope.identity.call_id);
        match self.ledger.submit_terminal(envelope)? {
            TerminalDisposition::Committed(_) => Ok(true),
            TerminalDisposition::Stale(_) => Ok(false),
        }
    }

    /// Classifies one inbound frame and, for a terminal, closes out the call.
    ///
    /// A request is registered on the same ledger the outbound calls use: the
    /// protocol makes a call id unique within an epoch in *both* directions, so
    /// a host that reuses one this side already spent is not a request that can
    /// be answered — a terminal on that id would be matched to the wrong call.
    pub fn accept_inbound(&mut self, envelope: &WireEnvelope) -> Result<Inbound, StateError> {
        match &envelope.message {
            WireMessage::Notification { method, payload } => {
                return Ok(Inbound::Notification {
                    identity: envelope.identity.clone(),
                    method: method.clone(),
                    payload: payload.clone(),
                })
            }
            WireMessage::Request { method, payload } => {
                // A request naming an incarnation that has already been
                // invalidated cannot be answered: the host's own ledger would
                // discard the terminal. Discarding it here is the contract.
                if self.ledger.classify_identity(&envelope.identity)?.is_some() {
                    return Ok(Inbound::Stale {
                        call_id: envelope.identity.call_id.clone(),
                    });
                }
                self.ledger.register_call(envelope.identity.clone())?;
                self.owed
                    .insert(envelope.identity.call_id.clone(), envelope.identity.clone());
                return Ok(Inbound::Request {
                    identity: envelope.identity.clone(),
                    method: method.clone(),
                    payload: payload.clone(),
                });
            }
            WireMessage::Chunk { payload } => {
                return match self.ledger.submit_chunk(envelope)? {
                    ChunkDisposition::Stale(_) => Ok(Inbound::Stale {
                        call_id: envelope.identity.call_id.clone(),
                    }),
                    ChunkDisposition::Accepted => Ok(Inbound::Chunk {
                        call_id: envelope.identity.call_id.clone(),
                        payload: payload.clone(),
                    }),
                }
            }
            WireMessage::Terminal { .. } => {}
        }
        match self.ledger.submit_terminal(envelope)? {
            TerminalDisposition::Stale(_) => Ok(Inbound::Stale {
                call_id: envelope.identity.call_id.clone(),
            }),
            TerminalDisposition::Committed(commit) => {
                self.pending.remove(&envelope.identity.call_id);
                let WireMessage::Terminal { payload, .. } = &envelope.message else {
                    unreachable!("checked above");
                };
                Ok(Inbound::Terminal {
                    call_id: envelope.identity.call_id.clone(),
                    status: commit.status,
                    payload: payload.clone(),
                })
            }
        }
    }

    /// Marks the host unusable and answers every call that was in flight.
    ///
    /// Idempotent: the first failure is the one that is reported, because it is
    /// the one that caused the rest. Calling it again returns nothing, since
    /// there is nothing left waiting.
    pub fn fail_all(&mut self, failure: HostFailure) -> Vec<WireEnvelope> {
        if self.failure.is_none() {
            self.failure = Some(failure.clone());
        }
        let failure = self.failure.clone().expect("set above");
        // Nothing is owed upstream any more: the process that asked is the one
        // that died.
        self.owed.clear();
        let payload = failure.payload();
        std::mem::take(&mut self.pending)
            .into_values()
            .map(|identity| {
                let envelope = WireEnvelope::new(
                    identity,
                    WireMessage::Terminal {
                        status: TerminalStatus::Error,
                        payload: payload.clone(),
                    },
                );
                // Committing keeps the ledger's at-most-one-terminal promise
                // true even for terminals the host never sent: a frame that
                // arrives later for the same call is a duplicate, not a race.
                let _ = self.ledger.submit_terminal(&envelope);
                envelope
            })
            .collect()
    }

    /// Starts over on a new epoch. Returns the terminals owed to whoever was
    /// waiting on the old host — a restart does not excuse leaving them hanging.
    ///
    /// Everything else is dropped on purpose: a new host has loaded no plugins,
    /// holds no subscriptions, and knows no scopes. Carrying that state across
    /// would describe a process that does not exist.
    pub fn restart(&mut self, host_epoch: u64) -> Result<Vec<WireEnvelope>, StateError> {
        let owed = self.fail_all(HostFailure::new("plugin host restarted"));
        self.host_epoch = host_epoch;
        self.ledger = CallLedger::new(host_epoch)?;
        register_control_scope(&mut self.ledger)?;
        self.registry = PluginRegistry::new();
        self.scopes.clear();
        self.pending.clear();
        self.owed.clear();
        self.next_call = 0;
        self.failure = None;
        Ok(owed)
    }
}

/// The reserved control scope exists for the whole epoch and never advances:
/// `platform/initialize` and `platform/shutdown` ride it, and the ledger will
/// not register a call whose scope it has never seen.
fn register_control_scope(ledger: &mut CallLedger) -> Result<(), StateError> {
    ledger.advance_scope_generation(
        PLATFORM_PLUGIN_ID,
        PLATFORM_CONTROL_SCOPE_ID,
        PLATFORM_CONTROL_GENERATION,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rebon_plugin_protocol::{EventSubscribeRequest, PluginLoadRequest, PluginReadyReport};

    use super::*;

    fn terminal(identity: &CallIdentity, status: TerminalStatus) -> WireEnvelope {
        WireEnvelope::new(
            identity.clone(),
            WireMessage::Terminal {
                status,
                payload: Payload::from(serde_json::json!({"ok": true})),
            },
        )
    }

    fn control(state: &mut SupervisorState) -> CallIdentity {
        let call_id = state.next_call_id();
        CallIdentity::platform_control(state.host_epoch(), call_id).unwrap()
    }

    #[test]
    fn a_terminal_closes_the_call_it_names() {
        let mut state = SupervisorState::new(7).unwrap();
        let identity = control(&mut state);
        state.begin_call(identity.clone()).unwrap();
        assert_eq!(state.pending_calls(), vec![identity.call_id.clone()]);

        let accepted = state
            .accept_inbound(&terminal(&identity, TerminalStatus::Success))
            .unwrap();
        match accepted {
            Inbound::Terminal {
                call_id, status, ..
            } => {
                assert_eq!(call_id, identity.call_id);
                assert_eq!(status, TerminalStatus::Success);
            }
            other => panic!("expected a terminal, got {other:?}"),
        }
        assert!(state.pending_calls().is_empty());
    }

    /// Host call ids come from the host, so the fixtures use its shape rather
    /// than this side's monotonic counter.
    fn host_request(call_id: &str, method: &str) -> WireEnvelope {
        WireEnvelope::new(
            CallIdentity::platform_control(7, call_id.to_owned()).unwrap(),
            WireMessage::Request {
                method: method.to_owned(),
                payload: Payload::null(),
            },
        )
    }

    #[test]
    fn a_request_from_the_host_is_never_mistaken_for_a_terminal() {
        let mut state = SupervisorState::new(7).unwrap();
        let mine = control(&mut state);
        state.begin_call(mine.clone()).unwrap();

        let accepted = state.accept_inbound(&host_request("h-1", "tool/invoke"));
        match accepted.unwrap() {
            Inbound::Request {
                identity, method, ..
            } => {
                assert_eq!(identity.call_id, "h-1");
                assert_eq!(method, "tool/invoke");
            }
            other => panic!("expected a request, got {other:?}"),
        }
        // The two directions are owed separately: this side still waits on its
        // own call, and now also owes an answer.
        assert_eq!(state.pending_calls(), vec![mine.call_id]);
        assert_eq!(state.owed_answers(), vec!["h-1".to_owned()]);
    }

    /// Answering is what discharges the debt, and the ledger is what makes a
    /// second answer impossible rather than merely unlikely.
    #[test]
    fn a_request_is_answered_exactly_once() {
        let mut state = SupervisorState::new(7).unwrap();
        let request = host_request("h-1", "event/subscribe");
        state.accept_inbound(&request).unwrap();

        let answer = terminal(&request.identity, TerminalStatus::Success);
        assert!(state.commit_inbound_terminal(&answer).unwrap());
        assert!(state.owed_answers().is_empty());
        assert!(state.commit_inbound_terminal(&answer).is_err());
    }

    /// The host's own ledger refuses a reused id too, so a request carrying one
    /// this side already spent means framing is no longer trustworthy.
    #[test]
    fn a_request_reusing_an_outbound_call_id_is_refused() {
        let mut state = SupervisorState::new(7).unwrap();
        let mine = control(&mut state);
        state.begin_call(mine.clone()).unwrap();

        let collision = host_request(&mine.call_id, "tool/invoke");
        assert!(state.accept_inbound(&collision).is_err());
    }

    /// Two plugins working the same session hold two independent generation
    /// counters. Closing one plugin's scope must not cancel the other's
    /// subscriptions: its own scope is still open, so the loss would be silent
    /// and permanent.
    #[test]
    fn closing_one_plugin_scope_leaves_another_plugins_session_alone() {
        let mut state = SupervisorState::new(7).unwrap();
        for plugin in ["plugin.a", "plugin.b"] {
            let request = PluginLoadRequest {
                plugin_id: plugin.into(),
                root: "/pkg".into(),
                entry: "index.mjs".into(),
                services: Vec::new(),
                event_topics: vec!["session".into()],
                published_topics: Vec::new(),
                llm_providers: Vec::new(),
                tools: Vec::new(),
                commands: Vec::new(),
                invokable_tools: Vec::new(),
                seats: Vec::new(),
                config: Payload::null(),
            };
            state.registry_mut().admit_load(&request).unwrap();
            state
                .registry_mut()
                .accept_ready(&PluginReadyReport {
                    plugin_id: plugin.into(),
                    services: Vec::new(),
                    event_topics: vec!["session".into()],
                    llm_providers: Vec::new(),
                    llm_adapters: Default::default(),
                    tools: Vec::new(),
                    commands: Vec::new(),
                })
                .unwrap();
            let identity = state.open_scope(plugin, "session-1").unwrap();
            state
                .registry_mut()
                .subscribe(
                    &identity,
                    &EventSubscribeRequest {
                        subscription: format!("{plugin}-sub"),
                        topic: "session".into(),
                    },
                )
                .unwrap();
        }

        let (_, revoked) = state.close_scope("plugin.a", "session-1").unwrap();
        assert_eq!(
            revoked,
            vec![("plugin.a".to_string(), "plugin.a-sub".to_string())]
        );
        assert!(state
            .registry()
            .subscription("plugin.b", "plugin.b-sub")
            .is_some());
    }

    /// A request on a scope incarnation that has already been invalidated is
    /// dropped: nothing can answer it, and a terminal on that identity would be
    /// discarded by the host anyway.
    #[test]
    fn a_request_from_a_closed_scope_incarnation_is_stale_not_fatal() {
        let mut state = SupervisorState::new(7).unwrap();
        let opened = state.open_scope("plugin.a", "session-1").unwrap();
        let stale = WireEnvelope::new(
            CallIdentity {
                call_id: "h-1".into(),
                ..opened.clone()
            },
            WireMessage::Request {
                method: "event/subscribe".into(),
                payload: Payload::null(),
            },
        );
        state.close_scope("plugin.a", "session-1").unwrap();

        assert!(matches!(
            state.accept_inbound(&stale).unwrap(),
            Inbound::Stale { .. }
        ));
        assert!(state.owed_answers().is_empty());
    }

    /// A notification owes nothing, so accepting one must not put an entry in
    /// the ledger that a later answer could be written against.
    #[test]
    fn a_notification_from_the_host_owes_no_answer() {
        let mut state = SupervisorState::new(7).unwrap();
        let envelope = WireEnvelope::new(
            CallIdentity::platform_control(7, "h-1".to_owned()).unwrap(),
            WireMessage::Notification {
                method: "call/cancel".into(),
                payload: Payload::null(),
            },
        );
        assert!(matches!(
            state.accept_inbound(&envelope).unwrap(),
            Inbound::Notification { .. }
        ));
        assert!(state.owed_answers().is_empty());
    }

    /// A dead host is not owed anything: the process that asked is the one that
    /// died, and holding the debt would only leak.
    #[test]
    fn a_host_failure_clears_what_was_owed_upstream() {
        let mut state = SupervisorState::new(7).unwrap();
        state
            .accept_inbound(&host_request("h-1", "tool/invoke"))
            .unwrap();
        assert_eq!(state.owed_answers(), vec!["h-1".to_owned()]);

        state.fail_all(HostFailure::new("host exited"));
        assert!(state.owed_answers().is_empty());
    }

    /// The rule the whole module exists for.
    #[test]
    fn every_call_in_flight_gets_a_terminal_when_the_host_dies() {
        let mut state = SupervisorState::new(7).unwrap();
        let first = control(&mut state);
        let second = control(&mut state);
        state.begin_call(first.clone()).unwrap();
        state.begin_call(second.clone()).unwrap();

        let owed = state.fail_all(
            HostFailure::new("host exited with code 2").with_diagnostics("SyntaxError: bad"),
        );
        assert_eq!(owed.len(), 2);
        let mut answered: Vec<&str> = owed
            .iter()
            .map(|envelope| envelope.identity.call_id.as_str())
            .collect();
        answered.sort();
        let mut expected = [first.call_id.as_str(), second.call_id.as_str()];
        expected.sort();
        assert_eq!(answered, expected);

        for envelope in &owed {
            let WireMessage::Terminal { status, payload } = &envelope.message else {
                panic!("fail-all must synthesise terminals");
            };
            assert_eq!(*status, TerminalStatus::Error);
            let value = payload.to_value().unwrap();
            assert_eq!(value["code"], HOST_FAILED_CODE);
            assert_eq!(value["message"], "host exited with code 2");
            assert_eq!(value["diagnostics"], "SyntaxError: bad");
        }
        assert!(state.pending_calls().is_empty());
        assert!(!state.is_alive());
    }

    /// The first failure is the cause; later ones are consequences of it.
    #[test]
    fn the_first_failure_is_the_one_reported() {
        let mut state = SupervisorState::new(7).unwrap();
        state.fail_all(HostFailure::new("stdout was not NDJSON"));
        state.fail_all(HostFailure::new("child exited"));
        assert_eq!(state.failure().unwrap().reason, "stdout was not NDJSON");
        assert!(state.fail_all(HostFailure::new("again")).is_empty());
    }

    #[test]
    fn a_dead_host_refuses_new_work_instead_of_queueing_it() {
        let mut state = SupervisorState::new(7).unwrap();
        state.fail_all(HostFailure::new("child exited"));
        let identity = CallIdentity::platform_control(7, "late").unwrap();
        assert!(matches!(
            state.begin_call(identity).unwrap_err(),
            StateError::HostFailed(_)
        ));
        assert!(matches!(
            state.open_scope("plugin.a", "session-1").unwrap_err(),
            StateError::HostFailed(_)
        ));
    }

    /// A terminal the host sent just before dying, arriving after the fail-all,
    /// must not resurrect the call or trip the at-most-one-terminal rule.
    #[test]
    fn a_terminal_racing_the_fail_all_is_a_duplicate_not_a_race() {
        let mut state = SupervisorState::new(7).unwrap();
        let identity = control(&mut state);
        state.begin_call(identity.clone()).unwrap();
        state.fail_all(HostFailure::new("child exited"));

        let late = state.accept_inbound(&terminal(&identity, TerminalStatus::Success));
        assert!(late.is_err(), "the slot was already committed");
        assert!(state.pending_calls().is_empty());
    }

    #[test]
    fn scope_generations_only_move_at_an_invalidation() {
        let mut state = SupervisorState::new(7).unwrap();
        let opened = state.open_scope("plugin.a", "session-1").unwrap();
        assert_eq!(opened.scope_generation, 0);

        let (closed, _) = state.close_scope("plugin.a", "session-1").unwrap();
        assert_eq!(closed.scope_generation, 1, "a close must advance");

        let reopened = state.open_scope("plugin.a", "session-1").unwrap();
        assert_eq!(
            reopened.scope_generation, 1,
            "reopening reuses the tombstone rather than advancing again"
        );
    }

    #[test]
    fn a_call_identity_rides_the_open_scopes_current_generation() {
        let mut state = SupervisorState::new(7).unwrap();
        state.open_scope("plugin.a", "session-1").unwrap();
        let identity = state.call_identity("plugin.a", "session-1").unwrap();
        assert_eq!(identity.scope_generation, 0);
        assert_eq!(identity.plugin_id, "plugin.a");

        state.close_scope("plugin.a", "session-1").unwrap();
        assert!(matches!(
            state.call_identity("plugin.a", "session-1").unwrap_err(),
            StateError::ScopeAlreadyClosed { .. }
        ));
        assert!(matches!(
            state.call_identity("plugin.a", "session-2").unwrap_err(),
            StateError::UnknownScope { .. }
        ));
    }

    #[test]
    fn closing_an_already_closed_scope_does_not_burn_a_generation() {
        let mut state = SupervisorState::new(7).unwrap();
        state.open_scope("plugin.a", "session-1").unwrap();
        state.close_scope("plugin.a", "session-1").unwrap();
        assert!(matches!(
            state.close_scope("plugin.a", "session-1").unwrap_err(),
            StateError::ScopeAlreadyClosed { .. }
        ));
        // …and the generation is where the first close left it.
        assert_eq!(
            state
                .open_scope("plugin.a", "session-1")
                .unwrap()
                .scope_generation,
            1
        );
    }

    #[test]
    fn closing_a_scope_that_was_never_opened_says_so() {
        let mut state = SupervisorState::new(7).unwrap();
        assert!(matches!(
            state.close_scope("plugin.a", "session-1").unwrap_err(),
            StateError::UnknownScope { .. }
        ));
    }

    #[test]
    fn call_ids_are_unique_within_an_epoch_and_name_it() {
        let mut state = SupervisorState::new(7).unwrap();
        let ids: Vec<String> = (0..3).map(|_| state.next_call_id()).collect();
        assert_eq!(ids, vec!["c7-1", "c7-2", "c7-3"]);
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3
        );
    }

    #[test]
    fn a_restart_answers_the_old_calls_and_forgets_everything_else() {
        let mut state = SupervisorState::new(7).unwrap();
        state.open_scope("plugin.a", "session-1").unwrap();
        let identity = control(&mut state);
        state.begin_call(identity.clone()).unwrap();

        let owed = state.restart(8).unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].identity.call_id, identity.call_id);
        assert_eq!(owed[0].identity.host_epoch, 7, "owed on the old epoch");

        assert_eq!(state.host_epoch(), 8);
        assert!(state.is_alive());
        assert!(state.pending_calls().is_empty());
        assert_eq!(state.next_call_id(), "c8-1");
        // A new host knows no scopes: closing one it never opened is an error,
        // not a silent success.
        assert!(state.close_scope("plugin.a", "session-1").is_err());
    }

    #[test]
    fn control_identities_use_the_reserved_namespace() {
        let mut state = SupervisorState::new(7).unwrap();
        let identity = control(&mut state);
        assert!(identity.is_platform_control());
        assert_eq!(identity.plugin_id, PLATFORM_PLUGIN_ID);
    }

    #[test]
    fn a_failure_renders_with_its_diagnostics() {
        let bare = HostFailure::new("child exited with code 3");
        assert_eq!(bare.to_string(), "child exited with code 3");
        assert_eq!(bare.diagnostics, None);

        let detailed = HostFailure::new("child exited with code 3").with_diagnostics("  ");
        assert_eq!(
            detailed.diagnostics, None,
            "blank stderr is not a diagnosis"
        );

        let detailed = HostFailure::new("child exited").with_diagnostics("Error: boom");
        assert_eq!(detailed.to_string(), "child exited: Error: boom");
    }
}
