//! # rebon-customselect — keyboard-navigable selection logic
//!
//! The list-selection logic behind every picker, dialog and menu: pure
//! reducers and pure projections, no IO and no rendering. Each module
//! pins its behaviour with a test table.
//!
//! ## Contents
//!
//! * [`option`] — `OptionEntry`, `InputOption`, `OptionType`: the
//!   option-with-description shape and the base option it is built on.
//! * [`option_map`] — index over the option list with
//!   first/last/previous/next sibling lookups and lookup by value.
//! * [`navigation`] — the keyboard navigation reducer (focus
//!   next/prev, page up/down, set-focus, reset), including the
//!   viewport-clamp math, wrap-around behaviour, focused-value
//!   validation and the visible-options window projection. Time
//!   advancement is explicit.
//! * [`select_state`] — single-select state: the selected value and the
//!   action that copies the focused value into it.
//! * [`select_input`] — the single-select keyboard-input reducer:
//!   every key handler (next, previous, accept, cancel, page up/down,
//!   numeric jump, space for multi-select, tab to toggle input mode,
//!   image-selection arrow handling).
//! * [`multi_select_state`] — the multi-select reducer: the
//!   selected-values list, the submit-button focus state machine,
//!   input-value map updates, tab/shift-tab navigation and the
//!   toggle-versus-submit Enter rules.
//! * [`select_layout`] — row projection for the `compact`, `expanded`
//!   and `compact-vertical` layouts, plus the highlight slicing and
//!   the index-width math.
//! * [`select_orchestrator`] — input-value map initialisation, the
//!   initial-value-versus-edited-field diff, and the disable-selection
//!   resolver that turns `hide_indexes` into a numeric-only disable.
//! * [`select_input_option`] — input-option compound logic:
//!   image-attachment counting, the `show_label` resolver and submit
//!   gating.
//! * [`select_multi`] — multi-select properties and their defaults.
//! * [`util`] — full-width digit / space normalisation helpers shared
//!   by both reducers.
//!
//! ## Outbound seam shapes
//!
//! External concerns are modelled as data and callbacks rather than
//! Cargo dependencies:
//!
//! 1. **Keybinding dispatch** → modelled as pre-resolved semantic
//!    events ([`select_input::SelectInputEvent`],
//!    [`multi_select_state::MultiSelectEvent`]). Mapping physical keys
//!    to those events is the consumer's job.
//!
//! 2. **Overlay registration and UI lifecycle** → the reducers emit
//!    actions and take no side effects; the consumer routes effects
//!    such as overlay registration, cancellation and callback
//!    invocation.
//!
//! 3. **List-item / byline / text-input rendering** → modelled as plain
//!    row-projection structs ([`select_layout::SelectRow`],
//!    [`select_input_option::InputOptionRow`]). The row contracts are
//!    owned locally, so no design-system dependency is needed.
//!
//! 4. **Clipboard image paste** → not performed here. The crate
//!    exposes the [`select_input_option::InputOptionEvent::ImagePaste`]
//!    variant the consumer drives, feeding the resulting clipboard data
//!    back through the reducer.
//!
//! ## Out of scope
//!
//! * **All rendering primitives** — the crate produces owned `String`s,
//!   structs and discriminated enums.
//! * **UI lifecycle plumbing** (effect/state/reducer/callback/memo
//!   hooks, input handlers, keybinding wiring) — the crate exposes
//!   pure reducers and pure projections.
//! * **Clipboard-read IO** — modelled as an event variant the consumer
//!   drives.
//! * **Cursor declaration plumbing** — the crate exposes a
//!   `declare_cursor` flag projection.
//! * **Fuzzy-match scoring** — this crate does not fuzzy-match; that is
//!   the consumer's filter.
//! * **String-width math** — modelled as a `Fn(&str) -> usize` callback
//!   the consumer injects when computing `compact-vertical` padding
//!   (defaulting to `str::chars().count()`).
//!
//! These are omitted rather than stubbed because misaligned stubs have
//! negative value: they hint at the wrong API and force downstream
//! consumers to either preserve the mistake or do a disruptive rename.
//!
//! ## Dependencies
//!
//! This crate has **zero dependencies on other `rebon-*` crates**.
//! Every option shape and every projection is owned in-tree, so it
//! compiles standalone.

#![deny(missing_docs)]

pub mod multi_select_state;
pub mod navigation;
pub mod option;
pub mod option_map;
pub mod select_input;
pub mod select_input_option;
pub mod select_layout;
pub mod select_multi;
pub mod select_orchestrator;
pub mod select_state;
pub mod util;

pub use multi_select_state::{MultiSelectAction, MultiSelectEvent, MultiSelectState};
pub use navigation::{
    NavigationAction, NavigationProps, NavigationState, VisibleOption, DEFAULT_VISIBLE_OPTION_COUNT,
};
pub use option::{
    BaseOption, InputBehaviour, OptionEntry, OptionId, OptionType, OptionWithDescription,
};
pub use option_map::{OptionMap, OptionMapItem};
pub use select_input::{DisableSelection, SelectInputAction, SelectInputEvent, SelectInputProps};
pub use select_input_option::{
    image_attachments_count, resolve_show_label, submit_decision, InputOptionEvent, InputOptionRow,
    SubmitDecision,
};
pub use select_layout::{
    compute_max_index_width, project_select_rows, SelectLayout, SelectRow, SelectRowKind,
};
pub use select_multi::{multi_default_value, SelectMultiProps};
pub use select_orchestrator::{
    initial_input_values, resolve_disable_selection, sync_initial_input_values,
    NumericDisableSelection,
};
pub use select_state::{SelectState, SelectStateAction};
pub use util::{normalize_full_width_digits, normalize_full_width_space};

#[cfg(test)]
mod compatibility {
    //! Compatibility canary — this crate must intentionally not depend
    //! on any other `rebon-*` crate; option shapes and row projections
    //! are defined locally. The test below reads the crate manifest and
    //! fails if a `rebon-*` dependency is ever added.

    /// Returns every dependency name declared by the crate manifest, across
    /// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` and
    /// their `[target.…]` and `[dependencies.<name>]` spellings.
    fn declared_dependency_names(manifest: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut in_dependency_table = false;
        for raw in manifest.lines() {
            let line = raw.trim();
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let header = header.trim();
                let mut segments = header.rsplit('.');
                let last = segments.next().unwrap_or("");
                let parent = segments.next().unwrap_or("");
                in_dependency_table = is_dependency_table(last);
                if is_dependency_table(parent) {
                    // `[dependencies.foo]` names the dependency in the header.
                    names.push(last.trim_matches('"').to_string());
                    in_dependency_table = false;
                }
                continue;
            }
            if !in_dependency_table || line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, _)) = line.split_once('=') {
                names.push(key.trim().trim_matches('"').to_string());
            }
        }
        names
    }

    fn is_dependency_table(header: &str) -> bool {
        matches!(
            header,
            "dependencies" | "dev-dependencies" | "build-dependencies"
        )
    }

    #[test]
    fn compatibility_with_prior_slices_is_disjoint() {
        let manifest = include_str!("../Cargo.toml");
        let rebon_dependencies: Vec<String> = declared_dependency_names(manifest)
            .into_iter()
            .filter(|name| name.starts_with("rebon"))
            .collect();
        assert!(
            rebon_dependencies.is_empty(),
            "this crate must not depend on any rebon-* crate, found: {rebon_dependencies:?}"
        );
    }

    #[test]
    fn declared_dependency_names_reads_every_dependency_table_spelling() {
        let manifest = concat!(
            "[package]\n",
            "name = \"x\"\n",
            "description = \"no rebon-* dependencies\"\n",
            "[dependencies]\n",
            "serde = \"1\"\n",
            "[dev-dependencies.rebon-types]\n",
            "path = \"../rebon-types\"\n",
            "[target.'cfg(windows)'.build-dependencies]\n",
            "winapi = \"0.3\"\n",
        );

        assert_eq!(
            declared_dependency_names(manifest),
            vec![
                "serde".to_string(),
                "rebon-types".to_string(),
                "winapi".to_string()
            ]
        );
    }
}
