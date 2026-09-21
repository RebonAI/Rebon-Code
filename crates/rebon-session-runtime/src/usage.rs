//! The session's usage ledger: the one place that counts what a session's
//! turns cost.
//!
//! A session's turns can run in this process or in a worker, and both are the
//! same session. Counting them on `AppState` made the count a property of a
//! screen: the worker builds its own `AppState` to answer a command with, so
//! `/cost` asked of a hosted session answered with the defaults of a state
//! nobody had ever run a turn through -- zeros, presented as facts. The
//! ledger lives on the session's engine half instead, where both writers can
//! reach it and where it outlives any one surface.
//!
//! The ledger counts; it does not price. `/cost` reads [`UsageSnapshot`] and
//! applies the model catalogue itself, which is why the per-model buckets are
//! keyed by `(provider, model)` and not folded into one number here.

use std::collections::BTreeMap;

use rebon_types::Usage;

/// An owned reading of the ledger.
///
/// Owned rather than borrowed on purpose: the ledger sits behind a mutex, and
/// a borrow of the map inside it cannot outlive the guard. Every reader wants
/// the whole thing anyway (the status line's JSON payload, `/cost`, `/context`),
/// and a session's model buckets are a handful of entries -- the clone is not
/// worth a lifetime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    /// Every turn of this session, added up.
    pub total: Usage,
    /// The turn that finished most recently. Not the largest, not a running
    /// sum -- what "last turn" means on screen.
    pub last_turn: Usage,
    /// Per `(provider, model)`, so a session that switched models can still
    /// be priced. `BTreeMap` because the order is rendered.
    pub by_model: BTreeMap<(String, String), Usage>,
    /// When the counting started, for the elapsed time `/cost` prints.
    pub started_at_ms: u64,
}

/// The session's running usage totals.
#[derive(Debug, Default)]
pub struct UsageLedger {
    total: Usage,
    last_turn: Usage,
    by_model: BTreeMap<(String, String), Usage>,
    started_at_ms: u64,
}

impl UsageLedger {
    /// The clock is passed in rather than read here: this module has no
    /// business knowing what time it is, and a test that pins the elapsed
    /// time is worth more than the saved argument.
    pub fn new(started_at_ms: u64) -> Self {
        Self {
            total: Usage::default(),
            last_turn: Usage::default(),
            by_model: BTreeMap::new(),
            started_at_ms,
        }
    }

    /// Fold one finished turn in.
    ///
    /// Called once per turn by whichever process ran it: the terminal from
    /// `prompt_lifecycle`, a worker from `finish_background_turn`. Both hand
    /// over the same `Usage` the model reported, so the same turn counted
    /// twice would double it -- one call site per surface, and no call from
    /// anything that merely observes a turn.
    ///
    /// The folding is [`add`], not `rebon_types::Usage::merge`: `merge` takes
    /// the non-zero fields of the newer reading, which is what a stream of
    /// deltas within one turn needs and the opposite of a running total.
    pub fn add_turn(
        &mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
        usage: Usage,
    ) {
        self.last_turn = usage;
        add(&mut self.total, usage);
        let bucket = self
            .by_model
            .entry((provider.into(), model.into()))
            .or_default();
        add(bucket, usage);
    }

    /// What the model has reported so far *within* the turn that is running.
    ///
    /// Not a turn: the provider streams a growing count, and only the last
    /// one is the turn's. So this replaces the running turn's numbers and
    /// touches no total -- the total is folded once, by [`Self::add_turn`],
    /// when the turn ends. A zero is a provider that did not say, and leaves
    /// the previous reading alone.
    pub fn note_streaming_usage(&mut self, input_tokens: u32, output_tokens: u32) {
        if input_tokens > 0 {
            self.last_turn.input_tokens = input_tokens;
        }
        if output_tokens > 0 {
            self.last_turn.output_tokens = output_tokens;
        }
    }

    /// Take the session total an owner published.
    ///
    /// A mirror runs none of the session's turns, so it has nothing to add
    /// up; what it has is the total the owner sends with its status. Adopting
    /// it rather than accumulating is the difference between showing the
    /// session's cost and showing the part of it this terminal happened to
    /// watch.
    pub fn adopt_owner_total(&mut self, total: Usage) {
        self.total = total;
    }

    /// Forget the running turn's numbers, keeping the session's totals.
    ///
    /// What a terminal does when the session on screen is replaced: the turn
    /// it was watching is not this screen's any more.
    pub fn clear_last_turn(&mut self) {
        self.last_turn = Usage::default();
    }

    /// Forget everything and start the clock again -- what `/clear` and
    /// `/cost reset` mean by a fresh session.
    pub fn reset(&mut self, started_at_ms: u64) {
        *self = Self::new(started_at_ms);
    }

    pub fn snapshot(&self) -> UsageSnapshot {
        UsageSnapshot {
            total: self.total,
            last_turn: self.last_turn,
            by_model: self.by_model.clone(),
            started_at_ms: self.started_at_ms,
        }
    }
}

/// Add one turn's counters into a running total.
///
/// Saturating rather than wrapping: a session large enough to overflow these
/// counters does not exist, and a pinned ceiling is a better failure than a
/// total that wraps to zero and reads as a session that cost nothing.
///
/// The billed totals come from the `Usage` helpers rather than the reported
/// fields, because what a provider bills for an input is not always what it
/// reports as one.
fn add(target: &mut Usage, usage: Usage) {
    target.input_tokens = target.input_tokens.saturating_add(usage.input_tokens);
    target.output_tokens = target.output_tokens.saturating_add(usage.output_tokens);
    target.total_input_tokens = target
        .total_input_tokens
        .saturating_add(usage.billed_input_tokens());
    target.total_output_tokens = target
        .total_output_tokens
        .saturating_add(usage.billed_output_tokens());
    target.cache_read_input_tokens = target
        .cache_read_input_tokens
        .saturating_add(usage.cache_read_input_tokens);
    target.cache_creation_input_tokens = target
        .cache_creation_input_tokens
        .saturating_add(usage.cache_creation_input_tokens);
    target.prompt_cache_hit_tokens = target
        .prompt_cache_hit_tokens
        .saturating_add(usage.prompt_cache_hit_tokens);
    target.prompt_cache_miss_tokens = target
        .prompt_cache_miss_tokens
        .saturating_add(usage.prompt_cache_miss_tokens);
    // The count this ledger replaced folded eight counters and dropped this
    // one. Nothing in the cli renders reasoning tokens from a total, so
    // folding it changes no output; what it changes is that the count exists
    // at all for whoever asks next.
    target.reasoning_tokens = target
        .reasoning_tokens
        .saturating_add(usage.reasoning_tokens);
}

#[cfg(any(test, feature = "test-support"))]
impl UsageLedger {
    /// A ledger already holding numbers a fixture chose.
    ///
    /// Tests about how a number is *rendered* should not have to
    /// reverse-engineer a turn that would produce it; tests about the
    /// counting itself use [`Self::add_turn`], like production does.
    /// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
    #[doc(hidden)]
    pub fn seeded(started_at_ms: u64, total: Usage, last_turn: Usage) -> Self {
        Self {
            total,
            last_turn,
            by_model: BTreeMap::new(),
            started_at_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u32, output: u32) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    #[test]
    fn a_turn_lands_in_the_total_the_last_turn_and_its_model_bucket() {
        let mut ledger = UsageLedger::new(1_000);

        ledger.add_turn("anthropic", "claude", usage(10, 3));

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.total.input_tokens, 10);
        assert_eq!(snapshot.total.output_tokens, 3);
        assert_eq!(snapshot.last_turn, usage(10, 3));
        assert_eq!(
            snapshot
                .by_model
                .get(&("anthropic".to_string(), "claude".to_string()))
                .map(|bucket| bucket.input_tokens),
            Some(10)
        );
        assert_eq!(snapshot.started_at_ms, 1_000);
    }

    #[test]
    fn turns_from_two_models_share_a_total_and_keep_separate_buckets() {
        let mut ledger = UsageLedger::new(0);

        ledger.add_turn("anthropic", "claude", usage(10, 3));
        ledger.add_turn("deepseek", "chat", usage(5, 1));

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.total.input_tokens, 15);
        assert_eq!(snapshot.total.output_tokens, 4);
        assert_eq!(snapshot.by_model.len(), 2);
        assert_eq!(
            snapshot.by_model[&("deepseek".to_string(), "chat".to_string())].input_tokens,
            5
        );
    }

    /// "Last turn" is the most recent one, not the largest and not a total:
    /// the status line reads it every frame and would otherwise creep.
    #[test]
    fn the_last_turn_is_replaced_not_accumulated() {
        let mut ledger = UsageLedger::new(0);

        ledger.add_turn("anthropic", "claude", usage(100, 40));
        ledger.add_turn("anthropic", "claude", usage(7, 2));

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.last_turn, usage(7, 2));
        assert_eq!(snapshot.total.input_tokens, 107);
    }

    /// A provider that reports only cache hit/miss (DeepSeek) must still
    /// price: the billed totals come from the helpers, not from the raw
    /// input count, so the cached half is not lost on the way into the total.
    #[test]
    fn cache_counters_survive_the_fold() {
        let mut ledger = UsageLedger::new(0);
        let cached = Usage {
            input_tokens: 0,
            output_tokens: 2,
            prompt_cache_hit_tokens: 900,
            prompt_cache_miss_tokens: 100,
            ..Usage::default()
        };

        ledger.add_turn("deepseek", "chat", cached);
        ledger.add_turn("deepseek", "chat", cached);

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.total.prompt_cache_hit_tokens, 1_800);
        assert_eq!(snapshot.total.prompt_cache_miss_tokens, 200);
        assert_eq!(
            snapshot.total.total_input_tokens,
            cached.billed_input_tokens().saturating_mul(2)
        );
    }

    #[test]
    fn reset_forgets_every_turn_and_restarts_the_clock() {
        let mut ledger = UsageLedger::new(1_000);
        ledger.add_turn("anthropic", "claude", usage(10, 3));

        ledger.reset(9_000);

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.total, Usage::default());
        assert_eq!(snapshot.last_turn, Usage::default());
        assert!(snapshot.by_model.is_empty());
        assert_eq!(snapshot.started_at_ms, 9_000);
    }

    /// The snapshot is a reading, not a handle: writing to the ledger after
    /// taking one must not change what the reader is holding.
    #[test]
    fn a_snapshot_does_not_move_under_the_reader() {
        let mut ledger = UsageLedger::new(0);
        ledger.add_turn("anthropic", "claude", usage(10, 3));
        let snapshot = ledger.snapshot();

        ledger.add_turn("anthropic", "claude", usage(1, 1));

        assert_eq!(snapshot.total.input_tokens, 10);
        assert_eq!(ledger.snapshot().total.input_tokens, 11);
    }
}
