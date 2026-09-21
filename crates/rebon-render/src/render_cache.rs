//! Bounded per-hunk render cache for pre-rendered diff output.
//!
//! Rendering a diff hunk — syntax highlighting every line and slicing it
//! into a gutter column and a content column — is expensive, so each hunk
//! keeps its last few results. A cache entry holds the full ANSI lines plus
//! the already-split gutter and content columns, keyed by the render
//! parameters that produced them, so a repeat render of the same hunk at the
//! same width costs one lookup instead of a fresh highlight and split.
//!
//! Four load-bearing properties:
//!
//! 1. **Per-hunk granularity.** Each hunk owns one cache keyed by a string of
//!    render parameters. [`HunkRenderCache`] is that single cache; the caller
//!    decides how hunks are identified — as a field on a per-hunk struct,
//!    behind an `Arc<Mutex<…>>`, or hashed into a
//!    `HashMap<HunkKey, HunkRenderCache>`.
//! 2. **Cap-and-clear, not LRU.** When a key arrives and the cache already
//!    holds [`RENDER_CACHE_PER_HUNK_CAP`] entries, the *entire* cache is
//!    dropped before the new entry is inserted. This is **not** an LRU
//!    eviction: in steady state the cache holds two widths × two dim variants
//!    (4 entries), and a fifth key only arrives while the terminal is being
//!    resized, when every existing width is already stale.
//! 3. **Cap fires BEFORE the new insert.** Concretely, with a cap of 4 a
//!    fifth distinct insert leaves exactly one entry — the new one.
//! 4. **A hit is a plain lookup.** [`HunkRenderCache::get`] takes `&self` and
//!    does not reorder or promote the entry it finds, so eviction stays
//!    insertion-ordered rather than LRU.
//!
//! ## Why a `Vec` instead of a `HashMap`
//!
//! Insertion order is the only order the cache keeps, and the cap-and-clear
//! path reads the entry count rather than iterating. With at most 4 entries a
//! `Vec<(String, CachedRender)>` is both faster (no hashing) and easier to
//! audit, and the `len() >= 4` check plus `Vec::clear` are as cheap as the
//! map operations they replace.

/// Hard cap on the number of entries one hunk's cache keeps.
pub const RENDER_CACHE_PER_HUNK_CAP: usize = 4;

/// Pre-split rendered output for one (theme, width, dim, …)
/// configuration.
///
/// `gutters` and `contents` are `None` when `gutter_width == 0`: the
/// terminal is too narrow for a gutter column, so the output stays
/// single-column and no split happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedRender {
    /// Full pre-rendered ANSI lines, top-to-bottom.
    pub lines: Vec<String>,
    /// Width of the gutter column. `0` when no split is in effect.
    pub gutter_width: usize,
    /// Pre-split gutter column. `None` when `gutter_width == 0`.
    pub gutters: Option<Vec<String>>,
    /// Pre-split content column. `None` when `gutter_width == 0`.
    pub contents: Option<Vec<String>>,
}

/// One `(key, value)` entry in the inner cap-and-clear cache.
/// Exposed so tests and consumers can iterate the current state for
/// debugging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkRenderEntry {
    pub key: String,
    pub render: CachedRender,
}

/// Cap-and-clear cache of rendered hunks. One instance belongs to one hunk;
/// the caller owns that per-hunk lifetime, e.g. by storing the cache as a
/// field on the hunk wrapper or by maintaining a `HashMap<HunkKey,
/// HunkRenderCache>` keyed on patch identity.
///
/// **Eviction is cap-and-clear, NOT LRU**: see the module docs for the
/// rationale.
#[derive(Debug, Default, Clone)]
pub struct HunkRenderCache {
    entries: Vec<HunkRenderEntry>,
    cap: usize,
}

impl HunkRenderCache {
    /// New empty cache with the default cap of
    /// [`RENDER_CACHE_PER_HUNK_CAP`].
    pub fn new() -> Self {
        Self::with_cap(RENDER_CACHE_PER_HUNK_CAP)
    }

    /// New empty cache with a custom cap. Useful for tests; the
    /// production caller always uses [`Self::new`].
    pub fn with_cap(cap: usize) -> Self {
        Self {
            entries: Vec::with_capacity(cap.min(16)),
            cap,
        }
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read-only view of the current entries, in insertion order. Used by
    /// tests rather than by the render path.
    pub fn entries(&self) -> &[HunkRenderEntry] {
        &self.entries
    }

    /// Cache lookup for one key. Returns a *clone* of the cached render
    /// rather than a reference, so callers can move the `Vec<String>` into a
    /// render target without holding the cache borrow.
    ///
    /// **No promotion on hit** — eviction is insertion-ordered
    /// cap-and-clear, not LRU. See the module docs for the rationale.
    pub fn get(&self, key: &str) -> Option<CachedRender> {
        self.entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.render.clone())
    }

    /// Insert (or update) a `(key, render)` pair, applying the cap first.
    ///
    /// Concrete behaviour:
    ///
    /// * If the key already exists, its value is updated **in place** and the
    ///   cap is **not** triggered: an update does not grow the cache past its
    ///   previous size.
    /// * If the key is new and the cache already has `cap` entries,
    ///   the entire map is cleared, then the new entry is inserted.
    ///   The cache then holds exactly 1 entry.
    /// * If the key is new and the map has fewer than `cap` entries,
    ///   the new entry is appended.
    pub fn insert(&mut self, key: String, render: CachedRender) {
        // In-place update — the entry count does not change when the key
        // already exists, so the cap-and-clear branch doesn't fire.
        if let Some(slot) = self.entries.iter_mut().find(|e| e.key == key) {
            slot.render = render;
            return;
        }

        // Cap-and-clear path. The test is `>=`, NOT `>`, so a cache that
        // already holds exactly `cap` entries clears before the new entry
        // is pushed.
        if self.entries.len() >= self.cap {
            self.entries.clear();
        }

        self.entries.push(HunkRenderEntry { key, render });
    }
}

/// Build the inner-cache key string: six fields joined by `|`.
///
/// * `theme`: the raw string.
/// * `width`: the unsigned integer, decimal.
/// * `dim`: `1` for `true`, `0` for `false` — deliberately not the strings
///   `"true"`/`"false"`, since the shorter form is wanted.
/// * `gutter_width`: the unsigned integer, decimal.
/// * `first_line`: the raw string, or `""` for `None`.
/// * `file_path`: the raw string.
///
/// The pipe separator is safe because it cannot occur inside any field:
/// themes are names like `dark`, paths use `/` or `\`, and the numeric
/// fields are pure digits.
pub fn build_cache_key(
    theme: &str,
    width: usize,
    dim: bool,
    gutter_width: usize,
    first_line: Option<&str>,
    file_path: &str,
) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        theme,
        width,
        if dim { 1 } else { 0 },
        gutter_width,
        first_line.unwrap_or(""),
        file_path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_render(tag: &str) -> CachedRender {
        CachedRender {
            lines: vec![format!("line-{tag}-1"), format!("line-{tag}-2")],
            gutter_width: 4,
            gutters: Some(vec![format!("g-{tag}-1"), format!("g-{tag}-2")]),
            contents: Some(vec![format!("c-{tag}-1"), format!("c-{tag}-2")]),
        }
    }

    fn no_split_render(tag: &str) -> CachedRender {
        CachedRender {
            lines: vec![format!("line-{tag}")],
            gutter_width: 0,
            gutters: None,
            contents: None,
        }
    }

    // -----------------------------------------------------------------
    // CachedRender shape
    // -----------------------------------------------------------------

    #[test]
    fn cached_render_with_split_columns() {
        let r = dummy_render("a");
        assert_eq!(r.gutter_width, 4);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.gutters.as_ref().unwrap().len(), 2);
        assert_eq!(r.contents.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn cached_render_without_split_columns() {
        let r = no_split_render("a");
        assert_eq!(r.gutter_width, 0);
        assert!(r.gutters.is_none());
        assert!(r.contents.is_none());
    }

    // -----------------------------------------------------------------
    // build_cache_key — field-by-field
    // -----------------------------------------------------------------

    #[test]
    fn cache_key_basic_shape() {
        let key = build_cache_key("dark", 80, false, 4, Some("#!/bin/sh"), "src/foo.rs");
        assert_eq!(key, "dark|80|0|4|#!/bin/sh|src/foo.rs");
    }

    #[test]
    fn cache_key_dim_projects_to_one() {
        let key = build_cache_key("dark", 80, true, 4, None, "x.rs");
        // dim=true → "1", first_line=None → "" (still occupies a slot
        // — six fields, five pipes — see below test for the explicit
        // empty-field shape).
        assert_eq!(key, "dark|80|1|4||x.rs");
    }

    #[test]
    fn cache_key_dim_projects_to_zero() {
        let key = build_cache_key("dark", 80, false, 4, None, "x.rs");
        assert_eq!(key, "dark|80|0|4||x.rs");
    }

    #[test]
    fn cache_key_gutter_width_zero_renders_as_zero() {
        let key = build_cache_key("light", 120, false, 0, None, "f.py");
        assert_eq!(key, "light|120|0|0||f.py");
    }

    #[test]
    fn cache_key_first_line_some_empty_string_is_empty_field() {
        let key = build_cache_key("dark", 80, false, 4, Some(""), "x.rs");
        // Some("") and None both render to ""
        assert_eq!(key, "dark|80|0|4||x.rs");
    }

    #[test]
    fn cache_key_first_line_some_renders_verbatim() {
        let key = build_cache_key("dark", 80, false, 4, Some("hello world"), "x.rs");
        assert_eq!(key, "dark|80|0|4|hello world|x.rs");
    }

    #[test]
    fn cache_key_distinguishes_themes() {
        let dark = build_cache_key("dark", 80, false, 4, None, "x.rs");
        let light = build_cache_key("light", 80, false, 4, None, "x.rs");
        assert_ne!(dark, light);
    }

    #[test]
    fn cache_key_distinguishes_widths() {
        let a = build_cache_key("dark", 80, false, 4, None, "x.rs");
        let b = build_cache_key("dark", 81, false, 4, None, "x.rs");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_distinguishes_dim_states() {
        let on = build_cache_key("dark", 80, true, 4, None, "x.rs");
        let off = build_cache_key("dark", 80, false, 4, None, "x.rs");
        assert_ne!(on, off);
    }

    #[test]
    fn cache_key_distinguishes_file_paths() {
        let a = build_cache_key("dark", 80, false, 4, None, "src/a.rs");
        let b = build_cache_key("dark", 80, false, 4, None, "src/b.rs");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_distinguishes_first_lines() {
        let a = build_cache_key("dark", 80, false, 4, Some("#!/bin/sh"), "x");
        let b = build_cache_key("dark", 80, false, 4, Some("#!/bin/bash"), "x");
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------
    // HunkRenderCache — basic mechanics
    // -----------------------------------------------------------------

    #[test]
    fn empty_cache_returns_none() {
        let cache = HunkRenderCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get("any-key"), None);
    }

    #[test]
    fn insert_then_get_returns_clone() {
        let mut cache = HunkRenderCache::new();
        cache.insert("k1".into(), dummy_render("a"));
        let got = cache.get("k1");
        assert_eq!(got, Some(dummy_render("a")));
        // Cache still holds the entry — get is non-mutating.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_grows_to_cap_then_holds_steady() {
        let mut cache = HunkRenderCache::new();
        cache.insert("k1".into(), dummy_render("1"));
        cache.insert("k2".into(), dummy_render("2"));
        cache.insert("k3".into(), dummy_render("3"));
        cache.insert("k4".into(), dummy_render("4"));
        assert_eq!(cache.len(), 4);
        // All four still present.
        assert!(cache.get("k1").is_some());
        assert!(cache.get("k2").is_some());
        assert!(cache.get("k3").is_some());
        assert!(cache.get("k4").is_some());
    }

    // -----------------------------------------------------------------
    // Cap-and-clear — the load-bearing behaviour
    // -----------------------------------------------------------------

    #[test]
    fn fifth_distinct_insert_clears_then_inserts() {
        let mut cache = HunkRenderCache::new();
        cache.insert("k1".into(), dummy_render("1"));
        cache.insert("k2".into(), dummy_render("2"));
        cache.insert("k3".into(), dummy_render("3"));
        cache.insert("k4".into(), dummy_render("4"));
        cache.insert("k5".into(), dummy_render("5"));
        // After cap-and-clear: only the new entry remains.
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("k1"), None);
        assert_eq!(cache.get("k2"), None);
        assert_eq!(cache.get("k3"), None);
        assert_eq!(cache.get("k4"), None);
        assert!(cache.get("k5").is_some());
    }

    #[test]
    fn sixth_insert_after_cap_grows_normally() {
        let mut cache = HunkRenderCache::new();
        for i in 1..=5 {
            cache.insert(format!("k{i}"), dummy_render(&i.to_string()));
        }
        // After 5 inserts: cleared then 1 entry.
        assert_eq!(cache.len(), 1);
        // Sixth insert just appends.
        cache.insert("k6".into(), dummy_render("6"));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("k5").is_some());
        assert!(cache.get("k6").is_some());
    }

    #[test]
    fn cap_triggers_at_exactly_cap_not_cap_plus_one() {
        // The condition is `>= cap`. With cap=4, after inserting exactly
        // 4 entries the next distinct insert clears. After inserting only
        // 3, the 4th does NOT clear.
        let mut cache = HunkRenderCache::new();
        cache.insert("a".into(), dummy_render("a"));
        cache.insert("b".into(), dummy_render("b"));
        cache.insert("c".into(), dummy_render("c"));
        cache.insert("d".into(), dummy_render("d"));
        // Still 4 — cap not triggered yet.
        assert_eq!(cache.len(), 4);
        // 5th insert triggers cap-and-clear.
        cache.insert("e".into(), dummy_render("e"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn duplicate_key_updates_in_place_and_does_not_trigger_cap() {
        let mut cache = HunkRenderCache::new();
        cache.insert("k1".into(), dummy_render("v1"));
        cache.insert("k2".into(), dummy_render("v2"));
        cache.insert("k3".into(), dummy_render("v3"));
        cache.insert("k4".into(), dummy_render("v4"));
        // Now full. Re-insert k4 with a new value.
        cache.insert("k4".into(), no_split_render("v4-updated"));
        assert_eq!(
            cache.len(),
            4,
            "in-place update of an existing key must NOT trigger cap-and-clear"
        );
        // The other three should still be there.
        assert!(cache.get("k1").is_some());
        assert!(cache.get("k2").is_some());
        assert!(cache.get("k3").is_some());
        // k4 carries the new value.
        let updated = cache.get("k4").unwrap();
        assert_eq!(updated.gutter_width, 0);
        assert_eq!(updated.lines, vec!["line-v4-updated"]);
    }

    #[test]
    fn custom_cap_triggers_at_custom_threshold() {
        let mut cache = HunkRenderCache::with_cap(2);
        cache.insert("a".into(), dummy_render("a"));
        cache.insert("b".into(), dummy_render("b"));
        assert_eq!(cache.len(), 2);
        cache.insert("c".into(), dummy_render("c"));
        // Cleared at 2, then inserted → 1.
        assert_eq!(cache.len(), 1);
        assert!(cache.get("c").is_some());
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_none());
    }

    #[test]
    fn cap_one_clears_on_every_distinct_insert() {
        let mut cache = HunkRenderCache::with_cap(1);
        cache.insert("a".into(), dummy_render("a"));
        assert_eq!(cache.len(), 1);
        cache.insert("b".into(), dummy_render("b"));
        // Cap triggered (1 >= 1) → clear → insert b. Still len 1.
        assert_eq!(cache.len(), 1);
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
    }

    // -----------------------------------------------------------------
    // get is non-mutating (FIFO cap-and-clear, not LRU)
    // -----------------------------------------------------------------

    #[test]
    fn get_does_not_promote_or_reorder() {
        let mut cache = HunkRenderCache::new();
        cache.insert("a".into(), dummy_render("a"));
        cache.insert("b".into(), dummy_render("b"));
        cache.insert("c".into(), dummy_render("c"));
        cache.insert("d".into(), dummy_render("d"));
        // Touch "a" — under LRU it would be promoted. Under
        // cap-and-clear it stays exactly where it is.
        let _ = cache.get("a");
        let _ = cache.get("a");
        // Now insert a 5th distinct key. The cap fires regardless of
        // any prior gets.
        cache.insert("e".into(), dummy_render("e"));
        assert_eq!(cache.len(), 1);
        assert!(cache.get("a").is_none());
    }

    #[test]
    fn get_miss_does_not_change_state() {
        let mut cache = HunkRenderCache::new();
        cache.insert("a".into(), dummy_render("a"));
        let before_len = cache.len();
        let _ = cache.get("nope");
        assert_eq!(cache.len(), before_len);
    }

    // -----------------------------------------------------------------
    // Default cap pinning
    // -----------------------------------------------------------------

    #[test]
    fn default_cap_is_four() {
        assert_eq!(RENDER_CACHE_PER_HUNK_CAP, 4);
        let cache = HunkRenderCache::new();
        assert_eq!(cache.cap, 4);
    }

    // -----------------------------------------------------------------
    // Four-variant steady state: two widths × dim on/off
    // -----------------------------------------------------------------

    #[test]
    fn four_variant_steady_state_holds_four_entries() {
        // Four variants (two widths × dim on/off) cover the steady state:
        // that exact sequence holds 4 entries with no clearing.
        let mut cache = HunkRenderCache::new();
        let entries = [
            build_cache_key("dark", 80, false, 4, None, "src/foo.rs"),
            build_cache_key("dark", 80, true, 4, None, "src/foo.rs"),
            build_cache_key("dark", 120, false, 4, None, "src/foo.rs"),
            build_cache_key("dark", 120, true, 4, None, "src/foo.rs"),
        ];
        for (i, key) in entries.iter().enumerate() {
            cache.insert(key.clone(), dummy_render(&i.to_string()));
        }
        assert_eq!(cache.len(), 4);
        for key in &entries {
            assert!(cache.get(key).is_some(), "missing entry for key {key}");
        }
    }
}
