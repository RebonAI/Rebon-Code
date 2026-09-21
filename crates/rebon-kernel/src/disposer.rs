use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::service::ServiceLease;

/// A single undo action produced by a registration.
///
/// Dropping a `Disposer` without running it does NOT undo the registration —
/// ownership of cleanup normally rests with the [`Scope`] it was pushed into.
pub struct Disposer(Option<Box<dyn FnOnce() + Send>>);

impl Disposer {
    pub fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(f)))
    }

    pub fn noop() -> Self {
        Self(None)
    }

    /// Run the disposal action now. Safe to call on a noop.
    pub fn dispose(mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

impl std::fmt::Debug for Disposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Disposer(armed)"
        } else {
            "Disposer(noop)"
        })
    }
}

/// One labelled entry in a [`Scope`]: the undo action plus a diagnostic
/// label describing what was registered (`on_json(llm/stream)`,
/// `provide(credentials)`, `fork(session-1)`, …).
struct ScopeEntry {
    label: std::sync::Arc<str>,
    disposer: Disposer,
    /// When set, the entry is prunable once the flag is true: the tracked
    /// subject (a child fork) already unwound itself, so keeping the entry
    /// would only make load/unload cycles grow the parent scope without
    /// bound. Pruning happens opportunistically on later pushes; running a
    /// pruned entry's disposer would have been a no-op anyway (idempotent).
    spent_when: Option<Arc<AtomicBool>>,
}

impl ScopeEntry {
    fn prunable(&self) -> bool {
        self.spent_when
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
}

/// Collects the disposers of one plugin (or one context fork) and unwinds
/// them in reverse registration order exactly once.
#[derive(Default)]
pub struct Scope {
    items: Mutex<Vec<ScopeEntry>>,
    disposed: Arc<AtomicBool>,
    /// Sticky `Closing`: set by [`Scope::close_leases`] and never cleared.
    /// The unload state machine only moves forward (`Live → Closing →
    /// Drained → Disposed`), so a lease minted after the close — by a
    /// straggling async registration, or by a fork made while draining —
    /// must be born closed rather than reopening the subtree.
    closing: AtomicBool,
    /// Child fork scopes, for subtree walks (lease close/drain). Weak: a
    /// child disposed and dropped on its own must not be pinned here.
    children: Mutex<Vec<Weak<Scope>>>,
    /// JSON-plane leases of registrations made through this scope's
    /// context. Weak: an entry replaced/removed drops out on its own.
    leases: Mutex<Vec<Weak<ServiceLease>>>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether [`Scope::dispose`] has already run.
    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::Acquire)
    }

    /// Whether [`Scope::close_leases`] has run on this scope: no lease it
    /// holds may serve a new call again.
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Add a disposer to this scope. If the scope was already disposed the
    /// registration is undone immediately — late registrations must not leak.
    pub fn push(&self, disposer: Disposer) {
        self.push_labeled("effect", disposer);
    }

    /// [`Scope::push`] with a diagnostic label recorded for
    /// [`Scope::live_labels`].
    pub fn push_labeled(&self, label: &str, disposer: Disposer) {
        self.push_entry(label, disposer, None);
    }

    /// [`Scope::push_labeled`] for entries tracking a subject that can
    /// unwind on its own (a child fork): once `spent_when` turns true the
    /// entry is prunable, keeping repeated load/unload cycles bounded.
    pub(crate) fn push_tracked(
        &self,
        label: &str,
        disposer: Disposer,
        spent_when: Arc<AtomicBool>,
    ) {
        self.push_entry(label, disposer, Some(spent_when));
    }

    fn push_entry(&self, label: &str, disposer: Disposer, spent_when: Option<Arc<AtomicBool>>) {
        if self.is_disposed() {
            disposer.dispose();
            return;
        }
        let mut items = self.items.lock().unwrap();
        // Re-check under the lock: dispose() may have drained concurrently.
        if self.is_disposed() {
            drop(items);
            disposer.dispose();
        } else {
            // Opportunistic pruning of entries whose subject already
            // unwound (order preserved — the LIFO discipline is intact,
            // pruned entries would have been no-ops).
            items.retain(|entry| !entry.prunable());
            items.push(ScopeEntry {
                label: std::sync::Arc::from(label),
                disposer,
                spent_when,
            });
        }
    }

    /// Share the disposed flag (for parent-side prunability tracking).
    pub(crate) fn disposed_flag(&self) -> Arc<AtomicBool> {
        self.disposed.clone()
    }

    /// Remember a child fork scope for subtree walks. Weak on purpose.
    pub(crate) fn track_child(&self, child: &Arc<Scope>) {
        {
            let mut children = self.children.lock().unwrap();
            children.retain(|weak| weak.strong_count() > 0);
            children.push(Arc::downgrade(child));
        }
        // Closing is a subtree property, and forks keep happening while a
        // drain runs. A child born under a closing parent inherits it, so
        // it cannot serve calls the parent has already refused.
        if self.is_closing() {
            child.close_leases();
        }
    }

    /// Remember a JSON-plane lease minted by a registration through this
    /// scope's context. On an already-disposed scope the lease closes
    /// immediately (the registration itself was undone by the late-push
    /// rule; its lease must refuse calls just the same).
    pub(crate) fn track_lease(&self, lease: &Arc<ServiceLease>) {
        if self.is_disposed() || self.is_closing() {
            lease.close();
            return;
        }
        let mut leases = self.leases.lock().unwrap();
        leases.retain(|weak| weak.strong_count() > 0);
        leases.push(Arc::downgrade(lease));
        // Re-check under the lock: a concurrent close_leases() may have
        // walked past this scope between the check and the push, and an
        // entry it never saw would keep serving after the unload.
        if self.is_closing() {
            lease.close();
        }
    }

    /// Close every JSON-plane lease in this scope's subtree: new calls on
    /// held references start refusing with the stable stale-provider error
    /// while already-running calls keep counting in
    /// [`Scope::lease_inflight`]. The Closing edge of the unload state
    /// machine (`Live → Closing`).
    pub fn close_leases(&self) {
        // Set first, walk second: a registration racing the walk then sees
        // the flag and closes its own lease (`track_lease`), instead of
        // slipping in behind us and reopening the subtree.
        self.closing.store(true, Ordering::Release);
        for lease in self.leases.lock().unwrap().iter().filter_map(Weak::upgrade) {
            lease.close();
        }
        for child in self
            .children
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            child.close_leases();
        }
    }

    /// In-flight JSON-plane calls across this scope's subtree — the drain
    /// observable (`Closing → Drained` waits for this to hit zero).
    pub fn lease_inflight(&self) -> u64 {
        let own: u64 = self
            .leases
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .map(|lease| lease.inflight())
            .sum();
        let children: u64 = self
            .children
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .map(|child| child.lease_inflight())
            .sum();
        own + children
    }

    /// Labels of all live (not yet disposed) registrations, in registration
    /// order. The enumeration half of the "registration leaves no residue"
    /// contract: mount, enumerate, dispose, assert empty.
    pub fn live_labels(&self) -> Vec<String> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .map(|entry| entry.label.to_string())
            .collect()
    }

    /// Number of live registrations in this scope.
    pub fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    /// Whether this scope currently holds no registrations.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Unwind all collected disposers in reverse order. Idempotent.
    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        let drained: Vec<ScopeEntry> = std::mem::take(&mut *self.items.lock().unwrap());
        for entry in drained.into_iter().rev() {
            entry.disposer.dispose();
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    #[test]
    fn disposes_in_reverse_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let scope = Scope::new();
        for i in 0..3 {
            let order = order.clone();
            scope.push(Disposer::new(move || order.lock().unwrap().push(i)));
        }
        scope.dispose();
        assert_eq!(*order.lock().unwrap(), vec![2, 1, 0]);
    }

    #[test]
    fn dispose_is_idempotent() {
        let count = Arc::new(AtomicUsize::new(0));
        let scope = Scope::new();
        let c = count.clone();
        scope.push(Disposer::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        scope.dispose();
        scope.dispose();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn late_push_disposes_immediately() {
        let count = Arc::new(AtomicUsize::new(0));
        let scope = Scope::new();
        scope.dispose();
        let c = count.clone();
        scope.push(Disposer::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
