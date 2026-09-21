//! Auto-mode denial record store — implements the "classifier deny ->
//! deny store -> /permissions approve/retry -> replay" chain.
//!
//! This module provides:
//! * a denial record with a stable id plus a ring buffer of capacity 20.
//! * the recent-denials list rendered inside `/permissions`.
//! * an id-based approve/retry selection set, so a concurrent `record()`
//!   cannot renumber pending choices.
//!
//! Storing only a human-readable `display` string per denial would make
//! real replay impossible — retry would be reduced to announcing the
//! commands in a notice rather than re-running them. This module avoids
//! that by capturing the full replay payload at the point of denial:
//!
//! * `tool_use_id` — the original invocation id (allows one-shot allow
//!   to land on the *same* call rather than a fresh one).
//! * `tool_name` + `tool_input` — the full invocation shape so the
//!   runtime can re-dispatch the exact call on retry.
//! * `status` — explicit `Pending`/`Approved`/`Retried`/`Resolved` so
//!   `/permissions` can distinguish "approve-only" from "approve and
//!   replay" intent, and the runtime can mark resolution.
//! * stable string ids — selecting by array index would drift under
//!   concurrent inserts, so each record gets a unique monotonically
//!   growing id that never reuses a slot.
//!
//! The store itself is pure data — it does not reach into a clock, a
//! JSON parser, or a runtime. Timestamps are provided by the caller
//! (`timestamp_ms`) and the `tool_input` payload is an opaque JSON
//! string the runtime forwards unchanged, so the bytes replayed on retry
//! are the bytes that were denied.

use std::collections::VecDeque;

/// Default cap for the denial ring buffer.
pub const AUTO_MODE_DENIAL_DEFAULT_CAPACITY: usize = 20;

/// Lifecycle state of a denial record.
///
/// * `Pending` — freshly recorded, awaiting user action in
///   `/permissions`.
/// * `Approved` — the user marked the denial for approval inside
///   `/permissions` (Enter). A one-shot allow has been installed for
///   this `tool_use_id` but no replay is pending.
/// * `Retried` — the user marked the denial for retry (`r`). The
///   runtime has been asked to replay the invocation.
/// * `Resolved` — the replay (or approve-only handling) has completed
///   and the record can be cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutoModeDenialStatus {
    Pending,
    Approved,
    Retried,
    Resolved,
}

impl AutoModeDenialStatus {
    pub fn as_wire(self) -> &'static str {
        match self {
            AutoModeDenialStatus::Pending => "pending",
            AutoModeDenialStatus::Approved => "approved",
            AutoModeDenialStatus::Retried => "retried",
            AutoModeDenialStatus::Resolved => "resolved",
        }
    }
}

/// Input a caller passes to [`AutoModeDenialStore::record`].
///
/// The store owns the id, so the caller only supplies the replay
/// payload and display metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeDenialInput {
    /// Invocation id the denial fired against. Same id the runtime
    /// keyed the pending permission check under — the allow path is keyed
    /// by that same id, so an approval lands on the original call rather
    /// than on a fresh one.
    pub tool_use_id: String,
    /// Tool name (e.g. `"Bash"`, `"Edit"`).
    pub tool_name: String,
    /// Opaque JSON text of the tool input. The store does not parse
    /// this — on retry the runtime re-dispatches the exact bytes.
    pub tool_input: String,
    /// Classifier reason string, unchanged from
    /// `PermissionDecisionReason::Classifier { reason }`.
    pub reason: String,
    /// Short human-readable summary for the `/permissions` list row.
    pub display: String,
    /// Caller-supplied timestamp. This crate keeps no clock, so the time
    /// of the denial comes in from outside.
    pub timestamp_ms: u64,
    /// Owning task id, if the invocation originated from a sub-agent.
    pub task_id: Option<String>,
    /// Owning conversation id, for multi-conversation surfaces.
    pub conversation_id: Option<String>,
}

/// A single denial record, with the full replay shape and a stable id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeDenial {
    pub id: String,
    pub tool_use_id: String,
    pub tool_name: String,
    pub tool_input: String,
    pub reason: String,
    pub display: String,
    pub timestamp_ms: u64,
    pub status: AutoModeDenialStatus,
    pub task_id: Option<String>,
    pub conversation_id: Option<String>,
}

impl AutoModeDenial {
    /// Shorthand — is this record still waiting on user action?
    pub fn is_pending(&self) -> bool {
        matches!(self.status, AutoModeDenialStatus::Pending)
    }

    /// Shorthand — has this record been handled (approved-only or
    /// retried, whether or not replay has finished)?
    pub fn is_handled(&self) -> bool {
        matches!(
            self.status,
            AutoModeDenialStatus::Approved
                | AutoModeDenialStatus::Retried
                | AutoModeDenialStatus::Resolved
        )
    }
}

/// Store of auto-mode denial records.
///
/// Newest records are at the *front* (index 0). When the store exceeds
/// its capacity the oldest record at the back is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeDenialStore {
    denials: VecDeque<AutoModeDenial>,
    next_seq: u64,
    capacity: usize,
}

impl Default for AutoModeDenialStore {
    fn default() -> Self {
        Self::with_capacity(AUTO_MODE_DENIAL_DEFAULT_CAPACITY)
    }
}

impl AutoModeDenialStore {
    /// Construct a store with an explicit capacity. Zero capacity is
    /// treated as 1 to avoid a silent drop-on-insert loop — callers
    /// that truly want to disable the store should simply not record.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            denials: VecDeque::with_capacity(capacity),
            next_seq: 0,
            capacity,
        }
    }

    /// Number of records currently held (any status).
    pub fn len(&self) -> usize {
        self.denials.len()
    }

    pub fn is_empty(&self) -> bool {
        self.denials.is_empty()
    }

    /// Configured maximum.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Record a new denial. Returns the assigned id.
    pub fn record(&mut self, input: AutoModeDenialInput) -> String {
        let id = self.allocate_id();
        let denial = AutoModeDenial {
            id: id.clone(),
            tool_use_id: input.tool_use_id,
            tool_name: input.tool_name,
            tool_input: input.tool_input,
            reason: input.reason,
            display: input.display,
            timestamp_ms: input.timestamp_ms,
            status: AutoModeDenialStatus::Pending,
            task_id: input.task_id,
            conversation_id: input.conversation_id,
        };
        self.denials.push_front(denial);
        while self.denials.len() > self.capacity {
            self.denials.pop_back();
        }
        id
    }

    /// Fetch a single record by id.
    pub fn get(&self, id: &str) -> Option<&AutoModeDenial> {
        self.denials.iter().find(|d| d.id == id)
    }

    /// Iterate over every record, newest-first.
    pub fn iter(&self) -> impl Iterator<Item = &AutoModeDenial> {
        self.denials.iter()
    }

    /// Iterate over pending records only, newest-first.
    pub fn pending(&self) -> impl Iterator<Item = &AutoModeDenial> {
        self.denials.iter().filter(|d| d.is_pending())
    }

    /// Transition a record to [`AutoModeDenialStatus::Approved`]. No-op
    /// if the record is missing or already in a terminal state.
    pub fn mark_approved(&mut self, id: &str) -> bool {
        self.transition(
            id,
            AutoModeDenialStatus::Approved,
            &[AutoModeDenialStatus::Pending],
        )
    }

    /// Transition a record to [`AutoModeDenialStatus::Retried`]. Valid
    /// from `Pending` or `Approved` (user may mark approve first and
    /// then upgrade to retry before closing the dialog).
    pub fn mark_retried(&mut self, id: &str) -> bool {
        self.transition(
            id,
            AutoModeDenialStatus::Retried,
            &[
                AutoModeDenialStatus::Pending,
                AutoModeDenialStatus::Approved,
            ],
        )
    }

    /// Transition a record to [`AutoModeDenialStatus::Resolved`] from
    /// any non-resolved state. Called by the runtime once replay has
    /// finished (successfully or not — the store does not track replay
    /// errors, those live in the tool-result transcript).
    pub fn resolve(&mut self, id: &str) -> bool {
        self.transition(
            id,
            AutoModeDenialStatus::Resolved,
            &[
                AutoModeDenialStatus::Pending,
                AutoModeDenialStatus::Approved,
                AutoModeDenialStatus::Retried,
            ],
        )
    }

    /// Drop a record by id. Returns `true` if removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.denials.len();
        self.denials.retain(|d| d.id != id);
        self.denials.len() != before
    }

    /// Drop every record currently in
    /// [`AutoModeDenialStatus::Resolved`]. Returns the number removed.
    pub fn clear_resolved(&mut self) -> usize {
        let before = self.denials.len();
        self.denials
            .retain(|d| d.status != AutoModeDenialStatus::Resolved);
        before - self.denials.len()
    }

    /// Drop every record regardless of status.
    pub fn clear_all(&mut self) {
        self.denials.clear();
    }

    fn transition(
        &mut self,
        id: &str,
        target: AutoModeDenialStatus,
        allowed_from: &[AutoModeDenialStatus],
    ) -> bool {
        if let Some(d) = self.denials.iter_mut().find(|d| d.id == id) {
            if allowed_from.contains(&d.status) {
                d.status = target;
                return true;
            }
        }
        false
    }

    fn allocate_id(&mut self) -> String {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        format!("auto-mode-denial-{seq}")
    }
}

/// Input for [`resolve_denials_for_permissions_close`].
///
/// Uses id-based `Vec<String>` selections (rather than index-based sets)
/// so concurrent record inserts cannot misalign the selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PermissionsCloseSelection {
    /// IDs the user marked with Enter (approve, no replay).
    pub approved_ids: Vec<String>,
    /// IDs the user marked with `r` (approve + replay). Retry
    /// subsumes approve — if an id appears in both sets,
    /// [`resolve_denials_for_permissions_close`] will treat it as
    /// retry.
    pub retry_ids: Vec<String>,
}

/// Replay payload emitted when a user chooses retry. The consumer (the
/// runtime) installs a one-shot allow for `tool_use_id` then dispatches
/// the stored invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialReplayRequest {
    pub denial_id: String,
    pub tool_use_id: String,
    pub tool_name: String,
    /// Opaque JSON text, unchanged from
    /// [`AutoModeDenial::tool_input`]. The runtime re-parses this at
    /// its own boundary.
    pub tool_input: String,
    pub reason: String,
    pub task_id: Option<String>,
    pub conversation_id: Option<String>,
}

/// Outcome of closing `/permissions` with a
/// [`PermissionsCloseSelection`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DenialsCloseOutcome {
    /// Invocations to replay, newest-first (matching store order).
    pub to_retry: Vec<DenialReplayRequest>,
    /// IDs that were approved without retry — the runtime should
    /// install a one-shot allow so a future identical call slides
    /// through, but nothing is re-dispatched immediately.
    pub approved_only_ids: Vec<String>,
    /// IDs the caller listed but that the store did not contain
    /// (already resolved, already removed, or never existed). Surfaced
    /// so the caller can log or inspect them rather than lose them.
    pub missing_ids: Vec<String>,
}

/// Translate the user's `/permissions`-close selection into concrete
/// side effects and apply the `Approved`/`Retried` status transitions
/// to the store.
///
/// Retry takes priority over approve: if an id is in both lists, it
/// emits a replay request and the record transitions to `Retried`. IDs
/// listed only in `approved_ids` transition to `Approved`. IDs not
/// found in the store are reported in `missing_ids` untouched.
pub fn resolve_denials_for_permissions_close(
    store: &mut AutoModeDenialStore,
    selection: &PermissionsCloseSelection,
) -> DenialsCloseOutcome {
    let mut outcome = DenialsCloseOutcome::default();

    let retry_set: std::collections::BTreeSet<&str> =
        selection.retry_ids.iter().map(String::as_str).collect();

    for id in &selection.retry_ids {
        match store.get(id).cloned() {
            Some(denial) => {
                if store.mark_retried(id) {
                    outcome.to_retry.push(DenialReplayRequest {
                        denial_id: denial.id,
                        tool_use_id: denial.tool_use_id,
                        tool_name: denial.tool_name,
                        tool_input: denial.tool_input,
                        reason: denial.reason,
                        task_id: denial.task_id,
                        conversation_id: denial.conversation_id,
                    });
                } else {
                    outcome.missing_ids.push(id.clone());
                }
            }
            None => outcome.missing_ids.push(id.clone()),
        }
    }

    for id in &selection.approved_ids {
        if retry_set.contains(id.as_str()) {
            continue;
        }
        match store.get(id) {
            Some(_) => {
                if store.mark_approved(id) {
                    outcome.approved_only_ids.push(id.clone());
                } else {
                    outcome.missing_ids.push(id.clone());
                }
            }
            None => outcome.missing_ids.push(id.clone()),
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_input(tag: &str) -> AutoModeDenialInput {
        AutoModeDenialInput {
            tool_use_id: format!("tool-{tag}"),
            tool_name: "Bash".to_string(),
            tool_input: format!("{{\"command\":\"echo {tag}\"}}"),
            reason: format!("classifier denied {tag}"),
            display: format!("denied {tag}"),
            timestamp_ms: 1_700_000_000 + tag.len() as u64,
            task_id: None,
            conversation_id: None,
        }
    }

    #[test]
    fn record_prepends_newest_and_assigns_stable_ids() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        let id_b = store.record(make_input("b"));
        assert_ne!(id_a, id_b);
        // Newest first.
        let ids: Vec<_> = store.iter().map(|d| d.id.clone()).collect();
        assert_eq!(ids, vec![id_b.clone(), id_a.clone()]);
        assert_eq!(
            store.get(&id_a).map(|d| d.display.as_str()),
            Some("denied a")
        );
        assert_eq!(
            store.get(&id_b).map(|d| d.display.as_str()),
            Some("denied b")
        );
    }

    #[test]
    fn record_respects_capacity_evicting_oldest() {
        let mut store = AutoModeDenialStore::with_capacity(3);
        let id_a = store.record(make_input("a"));
        store.record(make_input("b"));
        store.record(make_input("c"));
        let id_d = store.record(make_input("d"));
        assert_eq!(store.len(), 3);
        // Oldest (id_a) evicted.
        assert!(store.get(&id_a).is_none());
        // Newest is at front.
        assert_eq!(
            store.iter().next().map(|d| d.id.as_str()),
            Some(id_d.as_str())
        );
    }

    #[test]
    fn with_capacity_zero_still_permits_one_record() {
        let mut store = AutoModeDenialStore::with_capacity(0);
        assert!(store.capacity() >= 1);
        let id = store.record(make_input("a"));
        assert_eq!(store.len(), 1);
        assert!(store.get(&id).is_some());
        // Second insert evicts the first.
        store.record(make_input("b"));
        assert!(store.get(&id).is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn allocated_ids_never_repeat_even_after_eviction() {
        let mut store = AutoModeDenialStore::with_capacity(2);
        let id_a = store.record(make_input("a"));
        store.record(make_input("b"));
        store.record(make_input("c")); // evicts id_a
        let id_d = store.record(make_input("d"));
        // Even though id_a was evicted, next_seq kept going.
        assert_ne!(id_a, id_d);
        assert!(id_d.ends_with("-3"));
    }

    #[test]
    fn pending_iterator_excludes_handled() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        let id_b = store.record(make_input("b"));
        let id_c = store.record(make_input("c"));
        assert!(store.mark_approved(&id_a));
        assert!(store.mark_retried(&id_b));
        let pending: Vec<_> = store.pending().map(|d| d.id.clone()).collect();
        assert_eq!(pending, vec![id_c]);
    }

    #[test]
    fn mark_approved_and_retried_respect_transitions() {
        let mut store = AutoModeDenialStore::default();
        let id = store.record(make_input("a"));
        assert!(store.mark_approved(&id));
        assert_eq!(
            store.get(&id).map(|d| d.status),
            Some(AutoModeDenialStatus::Approved)
        );
        // Approved -> Retried is allowed (user upgrades choice).
        assert!(store.mark_retried(&id));
        assert_eq!(
            store.get(&id).map(|d| d.status),
            Some(AutoModeDenialStatus::Retried)
        );
        // Retried -> Approved is *not* allowed — don't silently demote.
        assert!(!store.mark_approved(&id));
        assert_eq!(
            store.get(&id).map(|d| d.status),
            Some(AutoModeDenialStatus::Retried)
        );
    }

    #[test]
    fn resolve_accepts_any_active_state_and_rejects_missing() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        let id_b = store.record(make_input("b"));
        let id_c = store.record(make_input("c"));
        store.mark_approved(&id_a);
        store.mark_retried(&id_b);
        assert!(store.resolve(&id_a));
        assert!(store.resolve(&id_b));
        assert!(store.resolve(&id_c));
        for id in [&id_a, &id_b, &id_c] {
            assert_eq!(
                store.get(id).map(|d| d.status),
                Some(AutoModeDenialStatus::Resolved)
            );
        }
        // Double-resolve is a no-op.
        assert!(!store.resolve(&id_a));
        // Resolving an unknown id reports false.
        assert!(!store.resolve("unknown"));
    }

    #[test]
    fn clear_resolved_only_drops_resolved_records() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        let id_b = store.record(make_input("b"));
        let id_c = store.record(make_input("c"));
        store.mark_approved(&id_a);
        store.resolve(&id_b);
        let dropped = store.clear_resolved();
        assert_eq!(dropped, 1);
        assert!(store.get(&id_b).is_none());
        assert!(store.get(&id_a).is_some());
        assert!(store.get(&id_c).is_some());
    }

    #[test]
    fn remove_and_clear_all_behave() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        let id_b = store.record(make_input("b"));
        assert!(store.remove(&id_a));
        assert!(!store.remove(&id_a));
        assert!(store.get(&id_a).is_none());
        assert!(store.get(&id_b).is_some());
        store.clear_all();
        assert!(store.is_empty());
        assert!(store.get(&id_b).is_none());
    }

    #[test]
    fn is_handled_covers_non_pending_states() {
        let mut store = AutoModeDenialStore::default();
        let id = store.record(make_input("a"));
        assert!(store.get(&id).unwrap().is_pending());
        store.mark_approved(&id);
        assert!(store.get(&id).unwrap().is_handled());
        store.mark_retried(&id);
        assert!(store.get(&id).unwrap().is_handled());
        store.resolve(&id);
        assert!(store.get(&id).unwrap().is_handled());
    }

    #[test]
    fn status_wire_strings_are_stable() {
        assert_eq!(AutoModeDenialStatus::Pending.as_wire(), "pending");
        assert_eq!(AutoModeDenialStatus::Approved.as_wire(), "approved");
        assert_eq!(AutoModeDenialStatus::Retried.as_wire(), "retried");
        assert_eq!(AutoModeDenialStatus::Resolved.as_wire(), "resolved");
    }

    #[test]
    fn resolve_close_retry_takes_priority_and_marks_retried() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        let id_b = store.record(make_input("b"));
        let selection = PermissionsCloseSelection {
            approved_ids: vec![id_a.clone(), id_b.clone()],
            retry_ids: vec![id_b.clone()],
        };
        let outcome = resolve_denials_for_permissions_close(&mut store, &selection);
        assert_eq!(outcome.to_retry.len(), 1);
        assert_eq!(outcome.to_retry[0].denial_id, id_b);
        assert_eq!(outcome.to_retry[0].tool_use_id, "tool-b");
        assert_eq!(outcome.approved_only_ids, vec![id_a.clone()]);
        assert!(outcome.missing_ids.is_empty());
        assert_eq!(
            store.get(&id_a).map(|d| d.status),
            Some(AutoModeDenialStatus::Approved)
        );
        assert_eq!(
            store.get(&id_b).map(|d| d.status),
            Some(AutoModeDenialStatus::Retried)
        );
    }

    #[test]
    fn resolve_close_reports_missing_ids() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        let selection = PermissionsCloseSelection {
            approved_ids: vec!["ghost".to_string()],
            retry_ids: vec![id_a.clone(), "phantom".to_string()],
        };
        let outcome = resolve_denials_for_permissions_close(&mut store, &selection);
        assert_eq!(outcome.to_retry.len(), 1);
        assert_eq!(outcome.to_retry[0].denial_id, id_a);
        assert!(outcome.approved_only_ids.is_empty());
        // Both non-matching ids reported, order preserved.
        assert_eq!(
            outcome.missing_ids,
            vec!["phantom".to_string(), "ghost".to_string()]
        );
    }

    #[test]
    fn resolve_close_skips_already_resolved_entries_via_missing_ids() {
        let mut store = AutoModeDenialStore::default();
        let id_a = store.record(make_input("a"));
        store.resolve(&id_a);
        let selection = PermissionsCloseSelection {
            approved_ids: vec![id_a.clone()],
            retry_ids: vec![id_a.clone()],
        };
        let outcome = resolve_denials_for_permissions_close(&mut store, &selection);
        assert!(outcome.to_retry.is_empty());
        assert!(outcome.approved_only_ids.is_empty());
        assert_eq!(outcome.missing_ids, vec![id_a.clone()]);
        // Status did not rewind.
        assert_eq!(
            store.get(&id_a).map(|d| d.status),
            Some(AutoModeDenialStatus::Resolved)
        );
    }

    #[test]
    fn replay_request_carries_full_payload() {
        let mut store = AutoModeDenialStore::default();
        let id = store.record(AutoModeDenialInput {
            tool_use_id: "call-42".to_string(),
            tool_name: "Edit".to_string(),
            tool_input: "{\"file\":\"x\"}".to_string(),
            reason: "auto-mode: edit outside cwd".to_string(),
            display: "Edit x".to_string(),
            timestamp_ms: 42,
            task_id: Some("task-1".to_string()),
            conversation_id: Some("conv-7".to_string()),
        });
        let selection = PermissionsCloseSelection {
            approved_ids: vec![],
            retry_ids: vec![id.clone()],
        };
        let outcome = resolve_denials_for_permissions_close(&mut store, &selection);
        let r = &outcome.to_retry[0];
        assert_eq!(r.denial_id, id);
        assert_eq!(r.tool_use_id, "call-42");
        assert_eq!(r.tool_name, "Edit");
        assert_eq!(r.tool_input, "{\"file\":\"x\"}");
        assert_eq!(r.reason, "auto-mode: edit outside cwd");
        assert_eq!(r.task_id.as_deref(), Some("task-1"));
        assert_eq!(r.conversation_id.as_deref(), Some("conv-7"));
    }

    #[test]
    fn resolve_close_preserves_replay_identity_fields() {
        let mut store = AutoModeDenialStore::default();
        let id = store.record(AutoModeDenialInput {
            tool_use_id: "tool-use-original".to_string(),
            tool_name: "Bash".to_string(),
            tool_input: "{\"command\":\"echo replay\",\"run_in_background\":false}".to_string(),
            reason: "classifier denied exact payload".to_string(),
            display: "Bash: echo replay".to_string(),
            timestamp_ms: 101,
            task_id: Some("agent-task-9".to_string()),
            conversation_id: Some("conversation-main".to_string()),
        });
        let outcome = resolve_denials_for_permissions_close(
            &mut store,
            &PermissionsCloseSelection {
                approved_ids: Vec::new(),
                retry_ids: vec![id.clone()],
            },
        );
        assert_eq!(outcome.to_retry.len(), 1);
        assert_eq!(outcome.missing_ids, Vec::<String>::new());
        let replay = &outcome.to_retry[0];
        assert_eq!(replay.denial_id, id);
        assert_eq!(replay.tool_use_id, "tool-use-original");
        assert_eq!(replay.tool_name, "Bash");
        assert_eq!(
            replay.tool_input,
            "{\"command\":\"echo replay\",\"run_in_background\":false}"
        );
        assert_eq!(replay.task_id.as_deref(), Some("agent-task-9"));
        assert_eq!(replay.conversation_id.as_deref(), Some("conversation-main"));
    }
}
