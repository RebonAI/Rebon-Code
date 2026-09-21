//! The kernel event plane.
//!
//! Four synchronous dispatch primitives cover the five known modes
//! (`serial` is the async flavour of `bail`; in a synchronous kernel they
//! are one primitive):
//!
//! | mode        | registration       | dispatch            | failure policy |
//! |-------------|--------------------|---------------------|----------------|
//! | `emit`      | `on`               | `emit`/`emit_scoped`| **isolated**: a panicking listener is contained and logged; the dispatcher and remaining listeners are unaffected |
//! | `bail`      | `bail`             | `bail_emit`         | propagates: policy deciders must not be silently skipped |
//! | `waterfall` | `wrap`             | `waterfall`         | propagates: same reason |
//! | `parallel`  | `on_parallel`      | `parallel`          | **isolated**: every listener runs; each outcome (value or panic) is collected |
//!
//! Observation modes (`emit`, `parallel`) isolate listener failures because
//! an observer must never break the emitter. Policy modes (`bail`,
//! `waterfall`) deliberately do NOT isolate: skipping a panicking decider
//! would silently change the decision.
//!
//! ## Scoped dispatch
//!
//! Every registration records the label path of the [`Context`](crate::Context)
//! it was made through. A scoped dispatch (`emit_scoped`, `parallel_scoped`,
//! and their JSON twins) carries the dispatcher's own path and reaches only
//! listeners whose origin lies on the same root-to-leaf path: ancestors
//! (including the root, the deliberate global-observer position) and
//! descendants of the dispatch scope see the event; sibling branches never
//! do. Two forks that share the same label path share a scope on purpose —
//! that is the join mechanism. Unscoped dispatch reaches every listener (a
//! deliberate registry-wide notification).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::Disposer;

/// Continuation handed to a typed waterfall middleware. Calling it runs the
/// rest of the chain; dropping it without calling short-circuits.
pub struct Next<'a, E: ?Sized, R> {
    inner: Box<dyn FnOnce(&E) -> R + 'a>,
}

impl<'a, E: ?Sized, R> Next<'a, E, R> {
    pub fn call(self, event: &E) -> R {
        (self.inner)(event)
    }
}

/// Continuation handed to a JSON waterfall middleware. The event value is
/// passed by value so middlewares may transform it before passing it on.
pub struct JsonNext<'a> {
    inner: Box<dyn FnOnce(serde_json::Value) -> serde_json::Value + 'a>,
}

impl<'a> JsonNext<'a> {
    pub fn call(self, event: serde_json::Value) -> serde_json::Value {
        (self.inner)(event)
    }
}

/// Live registration counts per dispatch table (diagnostics / leak
/// detection — the observability half of the "registration leaves no
/// residue" contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventBusStats {
    pub typed_on: usize,
    pub typed_bail: usize,
    pub typed_wrap: usize,
    pub typed_parallel: usize,
    pub json_on: usize,
    pub json_wrap: usize,
    pub json_parallel: usize,
}

impl EventBusStats {
    pub fn total(&self) -> usize {
        self.typed_on
            + self.typed_bail
            + self.typed_wrap
            + self.typed_parallel
            + self.json_on
            + self.json_wrap
            + self.json_parallel
    }
}

/// Whether two scope paths lie on the same root-to-leaf chain. The empty
/// path (the root) relates to everything.
fn scopes_related(a: &str, b: &str) -> bool {
    fn is_prefix(longer: &str, shorter: &str) -> bool {
        longer.len() > shorter.len()
            && longer.as_bytes()[shorter.len()] == b'/'
            && longer.starts_with(shorter)
    }
    a.is_empty() || b.is_empty() || a == b || is_prefix(a, b) || is_prefix(b, a)
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

type Erased = Arc<dyn Any + Send + Sync>;

struct Handler<F: ?Sized> {
    token: u64,
    /// Label path of the context this handler was registered through.
    origin: Arc<str>,
    f: Arc<F>,
}

impl<F: ?Sized> Clone for Handler<F> {
    fn clone(&self) -> Self {
        Self {
            token: self.token,
            origin: self.origin.clone(),
            f: self.f.clone(),
        }
    }
}

type OnFn = dyn Fn(&dyn Any) + Send + Sync;
type BailFn = dyn Fn(&dyn Any) -> Option<Erased> + Send + Sync;
/// Erased middleware: (event, rest-of-chain) -> erased result.
type WrapFn = dyn Fn(&dyn Any, Box<dyn FnOnce(&dyn Any) -> Erased + '_>) -> Erased + Send + Sync;
/// Erased parallel responder: `None` means the event failed to downcast.
type ParallelFn = dyn Fn(&dyn Any) -> Option<Erased> + Send + Sync;

type JsonOnFn = dyn Fn(&serde_json::Value) + Send + Sync;
type JsonWrapFn = dyn Fn(serde_json::Value, JsonNext<'_>) -> serde_json::Value + Send + Sync;
type JsonParallelFn =
    dyn Fn(&serde_json::Value) -> Result<serde_json::Value, crate::KernelError> + Send + Sync;

/// One dispatch table: event key → handlers in registration order.
type Table<K, F> = RwLock<HashMap<K, Vec<Handler<F>>>>;

/// The dispatcher-supplied innermost step of a typed waterfall chain,
/// consumed exactly once when the chain bottoms out.
type Terminal<'a, E, R> = Option<Box<dyn FnOnce(&E) -> R + 'a>>;

/// The erased rest-of-chain continuation a waterfall middleware receives.
type ErasedRest<'a> = Box<dyn FnOnce(&dyn Any) -> Erased + 'a>;

#[derive(Default)]
pub(crate) struct EventBus {
    on: Table<TypeId, OnFn>,
    // Keyed by (event type, result type) so the same event may participate
    // in bail/waterfall/parallel chains with different result types.
    bail: Table<(TypeId, TypeId), BailFn>,
    wrap: Table<(TypeId, TypeId), WrapFn>,
    parallel: Table<(TypeId, TypeId), ParallelFn>,
    json_on: Table<String, JsonOnFn>,
    json_wrap: Table<String, JsonWrapFn>,
    json_parallel: Table<String, JsonParallelFn>,
    next_token: AtomicU64,
}

/// Remove-by-token helper shared by all disposer closures.
fn remove<K: std::hash::Hash + Eq, F: ?Sized>(map: &Table<K, F>, key: &K, token: u64) {
    let mut map = map.write().unwrap();
    if let Some(list) = map.get_mut(key) {
        list.retain(|h| h.token != token);
        if list.is_empty() {
            map.remove(key);
        }
    }
}

fn count<K, F: ?Sized>(map: &Table<K, F>) -> usize {
    map.read().unwrap().values().map(Vec::len).sum()
}

impl EventBus {
    fn token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn stats(&self) -> EventBusStats {
        EventBusStats {
            typed_on: count(&self.on),
            typed_bail: count(&self.bail),
            typed_wrap: count(&self.wrap),
            typed_parallel: count(&self.parallel),
            json_on: count(&self.json_on),
            json_wrap: count(&self.json_wrap),
            json_parallel: count(&self.json_parallel),
        }
    }

    // ---- typed broadcast ----

    pub(crate) fn on<E: Any + Send + Sync>(
        self: &Arc<Self>,
        origin: Arc<str>,
        handler: impl Fn(&E) + Send + Sync + 'static,
    ) -> Disposer {
        let token = self.token();
        let erased: Arc<OnFn> = Arc::new(move |any| {
            if let Some(event) = any.downcast_ref::<E>() {
                handler(event);
            }
        });
        self.on
            .write()
            .unwrap()
            .entry(TypeId::of::<E>())
            .or_default()
            .push(Handler {
                token,
                origin,
                f: erased,
            });
        let weak = Arc::downgrade(self);
        Disposer::new(move || {
            if let Some(bus) = weak.upgrade() {
                remove(&bus.on, &TypeId::of::<E>(), token);
            }
        })
    }

    /// Broadcast. `scope: None` reaches every listener; `Some(path)`
    /// reaches listeners on the same root-to-leaf chain only. Each listener
    /// runs isolated: a panic is contained and logged, the dispatcher and
    /// the remaining listeners are unaffected.
    pub(crate) fn emit<E: Any + Send + Sync>(&self, scope: Option<&str>, event: &E) {
        let snapshot: Vec<Handler<OnFn>> = self
            .on
            .read()
            .unwrap()
            .get(&TypeId::of::<E>())
            .cloned()
            .unwrap_or_default();
        // Handlers run outside the lock so they may re-enter the bus.
        for handler in snapshot {
            if let Some(scope) = scope {
                if !scopes_related(&handler.origin, scope) {
                    continue;
                }
            }
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| (handler.f)(event))) {
                tracing::error!(
                    origin = %handler.origin,
                    event = %std::any::type_name::<E>(),
                    panic = %panic_message(payload),
                    "event listener panicked; contained"
                );
            }
        }
    }

    // ---- typed bail (first non-empty answer wins) ----
    //
    // `bail` is also the synchronous form of `serial`: listeners run in
    // registration order and the first `Some` stops the chain. A panic
    // propagates to the dispatcher — a policy decider must not be silently
    // skipped.

    pub(crate) fn bail<E: Any + Send + Sync, R: Any + Send + Sync>(
        self: &Arc<Self>,
        origin: Arc<str>,
        handler: impl Fn(&E) -> Option<R> + Send + Sync + 'static,
    ) -> Disposer {
        let token = self.token();
        let key = (TypeId::of::<E>(), TypeId::of::<R>());
        let erased: Arc<BailFn> = Arc::new(move |any| {
            let event = any.downcast_ref::<E>()?;
            handler(event).map(|r| Arc::new(r) as Erased)
        });
        self.bail
            .write()
            .unwrap()
            .entry(key)
            .or_default()
            .push(Handler {
                token,
                origin,
                f: erased,
            });
        let weak = Arc::downgrade(self);
        Disposer::new(move || {
            if let Some(bus) = weak.upgrade() {
                remove(&bus.bail, &key, token);
            }
        })
    }

    pub(crate) fn bail_emit<E: Any + Send + Sync, R: Any + Send + Sync>(
        &self,
        event: &E,
    ) -> Option<R> {
        let key = (TypeId::of::<E>(), TypeId::of::<R>());
        let snapshot: Vec<Handler<BailFn>> = self
            .bail
            .read()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        for handler in snapshot {
            if let Some(result) = (handler.f)(event) {
                if let Ok(result) = result.downcast::<R>() {
                    return Some(
                        Arc::try_unwrap(result)
                            .unwrap_or_else(|arc| panic!("bail result Arc still shared: {arc:p}")),
                    );
                }
            }
        }
        None
    }

    // ---- typed waterfall ----

    pub(crate) fn wrap<E: Any + Send + Sync, R: Any + Send + Sync>(
        self: &Arc<Self>,
        origin: Arc<str>,
        middleware: impl Fn(&E, Next<'_, E, R>) -> R + Send + Sync + 'static,
    ) -> Disposer {
        let token = self.token();
        let key = (TypeId::of::<E>(), TypeId::of::<R>());
        let erased: Arc<WrapFn> = Arc::new(move |any, rest| {
            let Some(event) = any.downcast_ref::<E>() else {
                return rest(any);
            };
            let next = Next {
                inner: Box::new(move |e: &E| {
                    let erased_result = rest(e as &dyn Any);
                    match erased_result.downcast::<R>() {
                        Ok(r) => Arc::try_unwrap(r)
                            .unwrap_or_else(|_| panic!("waterfall result Arc still shared")),
                        Err(_) => panic!("waterfall chain produced a mismatched result type"),
                    }
                }),
            };
            Arc::new(middleware(event, next)) as Erased
        });
        self.wrap
            .write()
            .unwrap()
            .entry(key)
            .or_default()
            .push(Handler {
                token,
                origin,
                f: erased,
            });
        let weak = Arc::downgrade(self);
        Disposer::new(move || {
            if let Some(bus) = weak.upgrade() {
                remove(&bus.wrap, &key, token);
            }
        })
    }

    pub(crate) fn waterfall<E: Any + Send + Sync, R: Any + Send + Sync>(
        &self,
        event: &E,
        terminal: impl FnOnce(&E) -> R,
    ) -> R {
        let key = (TypeId::of::<E>(), TypeId::of::<R>());
        let snapshot: Vec<Handler<WrapFn>> = self
            .wrap
            .read()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_default();

        fn run_chain<E: Any + Send + Sync, R: Any + Send + Sync>(
            chain: &[Handler<WrapFn>],
            event: &dyn Any,
            terminal: &mut Terminal<'_, E, R>,
        ) -> Erased {
            match chain.split_first() {
                None => {
                    let terminal = terminal.take().expect("terminal called twice");
                    let event = event
                        .downcast_ref::<E>()
                        .expect("waterfall event type changed mid-chain");
                    Arc::new(terminal(event)) as Erased
                }
                Some((head, tail)) => {
                    let rest: ErasedRest<'_> =
                        Box::new(move |e| run_chain::<E, R>(tail, e, terminal));
                    (head.f)(event, rest)
                }
            }
        }

        let mut terminal: Terminal<'_, E, R> = Some(Box::new(terminal));
        let erased = run_chain::<E, R>(&snapshot, event, &mut terminal);
        match erased.downcast::<R>() {
            Ok(r) => {
                Arc::try_unwrap(r).unwrap_or_else(|_| panic!("waterfall result Arc still shared"))
            }
            Err(_) => panic!("waterfall chain produced a mismatched result type"),
        }
    }

    // ---- typed parallel (all-run barrier) ----

    pub(crate) fn on_parallel<E: Any + Send + Sync, R: Any + Send + Sync>(
        self: &Arc<Self>,
        origin: Arc<str>,
        handler: impl Fn(&E) -> R + Send + Sync + 'static,
    ) -> Disposer {
        let token = self.token();
        let key = (TypeId::of::<E>(), TypeId::of::<R>());
        let erased: Arc<ParallelFn> = Arc::new(move |any| {
            let event = any.downcast_ref::<E>()?;
            Some(Arc::new(handler(event)) as Erased)
        });
        self.parallel
            .write()
            .unwrap()
            .entry(key)
            .or_default()
            .push(Handler {
                token,
                origin,
                f: erased,
            });
        let weak = Arc::downgrade(self);
        Disposer::new(move || {
            if let Some(bus) = weak.upgrade() {
                remove(&bus.parallel, &key, token);
            }
        })
    }

    /// All-run barrier: every matching listener runs even when some panic;
    /// each outcome is collected (`Err` carries the panic message). Ordering
    /// follows registration order.
    pub(crate) fn parallel<E: Any + Send + Sync, R: Any + Send + Sync>(
        &self,
        scope: Option<&str>,
        event: &E,
    ) -> Vec<Result<R, String>> {
        let key = (TypeId::of::<E>(), TypeId::of::<R>());
        let snapshot: Vec<Handler<ParallelFn>> = self
            .parallel
            .read()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let mut outcomes = Vec::with_capacity(snapshot.len());
        for handler in snapshot {
            if let Some(scope) = scope {
                if !scopes_related(&handler.origin, scope) {
                    continue;
                }
            }
            match catch_unwind(AssertUnwindSafe(|| (handler.f)(event))) {
                Ok(Some(erased)) => match erased.downcast::<R>() {
                    Ok(r) => outcomes.push(Ok(Arc::try_unwrap(r)
                        .unwrap_or_else(|_| panic!("parallel result Arc still shared")))),
                    Err(_) => outcomes.push(Err("mismatched parallel result type".into())),
                },
                Ok(None) => {} // foreign event shape; not this listener's
                Err(payload) => {
                    let message = panic_message(payload);
                    tracing::error!(
                        origin = %handler.origin,
                        event = %std::any::type_name::<E>(),
                        panic = %message,
                        "parallel listener panicked; outcome collected"
                    );
                    outcomes.push(Err(message));
                }
            }
        }
        outcomes
    }

    // ---- JSON plane ----

    pub(crate) fn on_json(
        self: &Arc<Self>,
        origin: Arc<str>,
        name: &str,
        handler: impl Fn(&serde_json::Value) + Send + Sync + 'static,
    ) -> Disposer {
        let token = self.token();
        self.json_on
            .write()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(Handler {
                token,
                origin,
                f: Arc::new(handler),
            });
        let weak = Arc::downgrade(self);
        let name = name.to_string();
        Disposer::new(move || {
            if let Some(bus) = weak.upgrade() {
                remove(&bus.json_on, &name, token);
            }
        })
    }

    /// Number of live JSON-plane listeners for an event name (diagnostics /
    /// leak detection — e.g. proving a disposed plugin left nothing behind).
    pub(crate) fn json_on_count(&self, name: &str) -> usize {
        self.json_on
            .read()
            .unwrap()
            .get(name)
            .map_or(0, |v| v.len())
    }

    pub(crate) fn emit_json(&self, scope: Option<&str>, name: &str, event: &serde_json::Value) {
        let snapshot: Vec<Handler<JsonOnFn>> = self
            .json_on
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default();
        for handler in snapshot {
            if let Some(scope) = scope {
                if !scopes_related(&handler.origin, scope) {
                    continue;
                }
            }
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| (handler.f)(event))) {
                tracing::error!(
                    origin = %handler.origin,
                    event = %name,
                    panic = %panic_message(payload),
                    "json event listener panicked; contained"
                );
            }
        }
    }

    pub(crate) fn wrap_json(
        self: &Arc<Self>,
        origin: Arc<str>,
        name: &str,
        middleware: impl Fn(serde_json::Value, JsonNext<'_>) -> serde_json::Value
            + Send
            + Sync
            + 'static,
    ) -> Disposer {
        let token = self.token();
        self.json_wrap
            .write()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(Handler {
                token,
                origin,
                f: Arc::new(middleware),
            });
        let weak = Arc::downgrade(self);
        let name = name.to_string();
        Disposer::new(move || {
            if let Some(bus) = weak.upgrade() {
                remove(&bus.json_wrap, &name, token);
            }
        })
    }

    /// `scope: None` runs every registered middleware; `Some(path)` runs
    /// only those on the same root-to-leaf chain, exactly like the scoped
    /// broadcast. A decision waterfall wants the scoped form: a middleware
    /// registered by one session must not answer another session's query.
    pub(crate) fn waterfall_json(
        &self,
        scope: Option<&str>,
        name: &str,
        event: serde_json::Value,
        terminal: impl FnOnce(serde_json::Value) -> serde_json::Value,
    ) -> serde_json::Value {
        let snapshot: Vec<Handler<JsonWrapFn>> = self
            .json_wrap
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default();
        let snapshot: Vec<Handler<JsonWrapFn>> = match scope {
            None => snapshot,
            Some(scope) => snapshot
                .into_iter()
                .filter(|handler| scopes_related(&handler.origin, scope))
                .collect(),
        };

        fn run_chain(
            chain: &[Handler<JsonWrapFn>],
            event: serde_json::Value,
            terminal: &mut Option<Box<dyn FnOnce(serde_json::Value) -> serde_json::Value + '_>>,
        ) -> serde_json::Value {
            match chain.split_first() {
                None => (terminal.take().expect("terminal called twice"))(event),
                Some((head, tail)) => {
                    let next = JsonNext {
                        inner: Box::new(move |e| run_chain(tail, e, terminal)),
                    };
                    (head.f)(event, next)
                }
            }
        }

        let mut terminal: Option<Box<dyn FnOnce(serde_json::Value) -> serde_json::Value + '_>> =
            Some(Box::new(terminal));
        run_chain(&snapshot, event, &mut terminal)
    }

    // ---- JSON parallel ----

    pub(crate) fn on_parallel_json(
        self: &Arc<Self>,
        origin: Arc<str>,
        name: &str,
        handler: impl Fn(&serde_json::Value) -> Result<serde_json::Value, crate::KernelError>
            + Send
            + Sync
            + 'static,
    ) -> Disposer {
        let token = self.token();
        self.json_parallel
            .write()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(Handler {
                token,
                origin,
                f: Arc::new(handler),
            });
        let weak = Arc::downgrade(self);
        let name = name.to_string();
        Disposer::new(move || {
            if let Some(bus) = weak.upgrade() {
                remove(&bus.json_parallel, &name, token);
            }
        })
    }

    pub(crate) fn parallel_json(
        &self,
        scope: Option<&str>,
        name: &str,
        event: &serde_json::Value,
    ) -> Vec<Result<serde_json::Value, String>> {
        let snapshot: Vec<Handler<JsonParallelFn>> = self
            .json_parallel
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default();
        let mut outcomes = Vec::with_capacity(snapshot.len());
        for handler in snapshot {
            if let Some(scope) = scope {
                if !scopes_related(&handler.origin, scope) {
                    continue;
                }
            }
            match catch_unwind(AssertUnwindSafe(|| (handler.f)(event))) {
                Ok(Ok(value)) => outcomes.push(Ok(value)),
                Ok(Err(err)) => outcomes.push(Err(err.to_string())),
                Err(payload) => {
                    let message = panic_message(payload);
                    tracing::error!(
                        origin = %handler.origin,
                        event = %name,
                        panic = %message,
                        "json parallel listener panicked; outcome collected"
                    );
                    outcomes.push(Err(message));
                }
            }
        }
        outcomes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Ping(u32);

    fn origin(path: &str) -> Arc<str> {
        Arc::from(path)
    }

    /// Silence the default panic hook for tests that intentionally panic
    /// inside listeners; restores the previous hook on drop.
    struct QuietPanics;
    impl QuietPanics {
        fn install() -> Self {
            std::panic::set_hook(Box::new(|_| {}));
            QuietPanics
        }
    }
    impl Drop for QuietPanics {
        fn drop(&mut self) {
            let _ = std::panic::take_hook();
        }
    }

    #[test]
    fn emit_broadcasts_to_all() {
        let bus = Arc::new(EventBus::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s1, s2) = (seen.clone(), seen.clone());
        let _d1 = bus.on::<Ping>(origin(""), move |p| s1.lock().unwrap().push(p.0 * 10));
        let _d2 = bus.on::<Ping>(origin(""), move |p| s2.lock().unwrap().push(p.0 * 100));
        bus.emit(None, &Ping(3));
        assert_eq!(*seen.lock().unwrap(), vec![30, 300]);
    }

    #[test]
    fn emit_isolates_a_panicking_listener() {
        let _quiet = QuietPanics::install();
        let bus = Arc::new(EventBus::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s1 = seen.clone();
        let _d1 = bus.on::<Ping>(origin("a"), move |p| s1.lock().unwrap().push(p.0));
        let _d2 = bus.on::<Ping>(origin("b"), |_: &Ping| panic!("listener exploded"));
        let s3 = seen.clone();
        let _d3 = bus.on::<Ping>(origin("c"), move |p| s3.lock().unwrap().push(p.0 + 100));

        // The dispatcher must not observe the panic, and the listener after
        // the panicking one must still run.
        bus.emit(None, &Ping(7));
        assert_eq!(*seen.lock().unwrap(), vec![7, 107]);
    }

    #[test]
    fn scoped_emit_reaches_chain_not_siblings() {
        let bus = Arc::new(EventBus::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        // Dropping a Disposer without running it keeps the registration
        // (ownership of cleanup rests with a Scope) — no need to hold them.
        for (who, path) in [
            ("root", ""),
            ("session", "compose/s1"),
            ("deep", "compose/s1/tool"),
            ("sibling", "compose/s2"),
            ("lookalike", "compose/s1x"),
        ] {
            let s = seen.clone();
            let _ = bus.on::<Ping>(origin(path), move |_| s.lock().unwrap().push(who));
        }
        bus.emit(Some("compose/s1"), &Ping(1));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["root", "session", "deep"],
            "ancestors and subtree see the event; sibling branches never do"
        );
    }

    #[test]
    fn disposer_unsubscribes() {
        let bus = Arc::new(EventBus::default());
        let seen = Arc::new(Mutex::new(0u32));
        let s = seen.clone();
        let d = bus.on::<Ping>(origin(""), move |p| *s.lock().unwrap() += p.0);
        bus.emit(None, &Ping(1));
        d.dispose();
        bus.emit(None, &Ping(1));
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[test]
    fn bail_returns_first_answer() {
        let bus = Arc::new(EventBus::default());
        let _d1 = bus.bail::<Ping, String>(origin(""), |p| (p.0 > 5).then(|| "big".to_string()));
        let _d2 = bus.bail::<Ping, String>(origin(""), |_| Some("fallback".to_string()));
        assert_eq!(bus.bail_emit::<Ping, String>(&Ping(9)).unwrap(), "big");
        assert_eq!(bus.bail_emit::<Ping, String>(&Ping(1)).unwrap(), "fallback");
    }

    #[test]
    fn bail_none_when_no_handler_answers() {
        let bus = Arc::new(EventBus::default());
        let _d = bus.bail::<Ping, String>(origin(""), |_| None);
        assert!(bus.bail_emit::<Ping, String>(&Ping(1)).is_none());
    }

    #[test]
    fn waterfall_wraps_in_registration_order() {
        let bus = Arc::new(EventBus::default());
        let _d1 = bus.wrap::<Ping, String>(origin(""), |e, next| format!("a({})", next.call(e)));
        let _d2 = bus.wrap::<Ping, String>(origin(""), |e, next| format!("b({})", next.call(e)));
        let out = bus.waterfall(&Ping(7), |p| format!("core:{}", p.0));
        assert_eq!(out, "a(b(core:7))");
    }

    #[test]
    fn waterfall_short_circuits_when_next_is_dropped() {
        let bus = Arc::new(EventBus::default());
        let _d1 = bus.wrap::<Ping, String>(origin(""), |_, _next| "blocked".to_string());
        let out = bus.waterfall::<Ping, String>(&Ping(7), |_| panic!("terminal must not run"));
        assert_eq!(out, "blocked");
    }

    #[test]
    fn parallel_collects_every_outcome_including_panics() {
        let _quiet = QuietPanics::install();
        let bus = Arc::new(EventBus::default());
        let _d1 = bus.on_parallel::<Ping, u32>(origin("a"), |p| p.0 * 2);
        let _d2 = bus.on_parallel::<Ping, u32>(origin("b"), |_| panic!("responder exploded"));
        let _d3 = bus.on_parallel::<Ping, u32>(origin("c"), |p| p.0 + 1);

        let outcomes = bus.parallel::<Ping, u32>(None, &Ping(10));
        assert_eq!(outcomes.len(), 3, "every listener runs, panics included");
        assert_eq!(outcomes[0], Ok(20));
        assert_eq!(outcomes[1], Err("responder exploded".to_string()));
        assert_eq!(outcomes[2], Ok(11));
    }

    #[test]
    fn scoped_parallel_filters_like_scoped_emit() {
        let bus = Arc::new(EventBus::default());
        let _d1 = bus.on_parallel::<Ping, &'static str>(origin(""), |_| "root");
        let _d2 = bus.on_parallel::<Ping, &'static str>(origin("a/s1"), |_| "mine");
        let _d3 = bus.on_parallel::<Ping, &'static str>(origin("a/s2"), |_| "sibling");
        let outcomes = bus.parallel::<Ping, &'static str>(Some("a/s1"), &Ping(1));
        assert_eq!(outcomes, vec![Ok("root"), Ok("mine")]);
    }

    #[test]
    fn handlers_may_reenter_the_bus() {
        let bus = Arc::new(EventBus::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        struct Inner(u32);
        let s_inner = seen.clone();
        let _d_inner = bus.on::<Inner>(origin(""), move |i| s_inner.lock().unwrap().push(i.0));
        let bus2 = bus.clone();
        let _d_outer = bus.on::<Ping>(origin(""), move |p| bus2.emit(None, &Inner(p.0 + 1)));
        bus.emit(None, &Ping(1));
        assert_eq!(*seen.lock().unwrap(), vec![2]);
    }

    #[test]
    fn json_plane_emit_and_waterfall() {
        let bus = Arc::new(EventBus::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let _d1 = bus.on_json(origin(""), "greet", move |v| {
            s.lock()
                .unwrap()
                .push(v["who"].as_str().unwrap().to_string());
        });
        bus.emit_json(None, "greet", &serde_json::json!({"who": "js"}));
        assert_eq!(*seen.lock().unwrap(), vec!["js".to_string()]);

        let _d2 = bus.wrap_json(origin(""), "save", |mut v, next| {
            v["touched"] = serde_json::json!(true);
            next.call(v)
        });
        let out = bus.waterfall_json(None, "save", serde_json::json!({}), |v| v);
        assert_eq!(out["touched"], true);
    }

    /// A decision waterfall must not let one session's middleware answer
    /// another's query: scoped dispatch filters exactly like scoped emit.
    #[test]
    fn scoped_json_waterfall_skips_sibling_middlewares() {
        let bus = Arc::new(EventBus::default());
        for (who, path) in [
            ("root", ""),
            ("mine", "compose/s1"),
            ("deep", "compose/s1/tool"),
            ("sibling", "compose/s2"),
        ] {
            let _ = bus.wrap_json(origin(path), "ask", move |mut v, next| {
                v["seen"] = serde_json::json!(format!(
                    "{}{who}",
                    v["seen"]
                        .as_str()
                        .map(|s| format!("{s},"))
                        .unwrap_or_default()
                ));
                next.call(v)
            });
        }
        let scoped = bus.waterfall_json(Some("compose/s1"), "ask", serde_json::json!({}), |v| v);
        assert_eq!(scoped["seen"], "root,mine,deep");
        let unscoped = bus.waterfall_json(None, "ask", serde_json::json!({}), |v| v);
        assert_eq!(unscoped["seen"], "root,mine,deep,sibling");
    }

    #[test]
    fn json_emit_isolates_panics_and_scopes() {
        let _quiet = QuietPanics::install();
        let bus = Arc::new(EventBus::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s1 = seen.clone();
        let _d1 = bus.on_json(origin("s1"), "evt", move |_| s1.lock().unwrap().push("s1"));
        let _d2 = bus.on_json(origin("s1"), "evt", |_| panic!("json listener exploded"));
        let s3 = seen.clone();
        let _d3 = bus.on_json(origin("s2"), "evt", move |_| s3.lock().unwrap().push("s2"));

        bus.emit_json(Some("s1"), "evt", &serde_json::json!({}));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["s1"],
            "sibling filtered, panic contained"
        );
    }

    #[test]
    fn json_parallel_collects_values_errors_and_panics() {
        let _quiet = QuietPanics::install();
        let bus = Arc::new(EventBus::default());
        let _d1 = bus.on_parallel_json(origin(""), "flush", |v| {
            Ok(serde_json::json!({ "seen": v["n"] }))
        });
        let _d2 = bus.on_parallel_json(origin(""), "flush", |_| {
            Err(crate::KernelError::Other("declined".into()))
        });
        let _d3 = bus.on_parallel_json(origin(""), "flush", |_| panic!("flush listener exploded"));

        let outcomes = bus.parallel_json(None, "flush", &serde_json::json!({"n": 5}));
        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0], Ok(serde_json::json!({"seen": 5})));
        assert!(outcomes[1].as_ref().unwrap_err().contains("declined"));
        assert!(outcomes[2].as_ref().unwrap_err().contains("exploded"));
    }

    #[test]
    fn stats_track_every_table_and_return_to_zero() {
        let bus = Arc::new(EventBus::default());
        assert_eq!(bus.stats().total(), 0);
        let d1 = bus.on::<Ping>(origin(""), |_| {});
        let d2 = bus.bail::<Ping, u32>(origin(""), |_| None);
        let d3 = bus.wrap::<Ping, u32>(origin(""), |e, next| next.call(e));
        let d4 = bus.on_parallel::<Ping, u32>(origin(""), |p| p.0);
        let d5 = bus.on_json(origin(""), "e", |_| {});
        let d6 = bus.wrap_json(origin(""), "e", |v, next| next.call(v));
        let d7 = bus.on_parallel_json(origin(""), "e", |v| Ok(v.clone()));
        let stats = bus.stats();
        assert_eq!(
            stats,
            EventBusStats {
                typed_on: 1,
                typed_bail: 1,
                typed_wrap: 1,
                typed_parallel: 1,
                json_on: 1,
                json_wrap: 1,
                json_parallel: 1,
            }
        );
        for d in [d1, d2, d3, d4, d5, d6, d7] {
            d.dispose();
        }
        assert_eq!(bus.stats().total(), 0, "disposal leaves no residue");
    }

    #[test]
    fn scope_relation_respects_path_boundaries() {
        assert!(scopes_related("", "a/b"));
        assert!(scopes_related("a/b", ""));
        assert!(scopes_related("a", "a"));
        assert!(scopes_related("a", "a/b"));
        assert!(scopes_related("a/b/c", "a/b"));
        assert!(!scopes_related("a/b", "a/c"));
        assert!(
            !scopes_related("ab", "a"),
            "prefix must end at a path boundary"
        );
        assert!(!scopes_related("a", "ab"));
    }
}
