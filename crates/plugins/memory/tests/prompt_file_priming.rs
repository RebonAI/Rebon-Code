//! The `MEMORY.md` half of the turn's read-state priming, contributed by this
//! plugin's prompt section.
//!
//! `rebon-core::system_prompt::prime_injected_prompt_files` seeds the cache
//! with every file the prompt quoted verbatim, so `Edit` and `Write` do not
//! refuse a file the model has already been shown. The engine's own half is
//! the project instruction documents; this crate's half is the auto-memory
//! entrypoints, reported through the prompt seat's `injected_files`. These
//! entrypoints, reported through the prompt seat's `injected_files`. These
//! tests cover the same rule `rebon-core`'s `system_prompt` module covers for
//! the instruction files, except for going through the seat.
//!
//! The one rule under test: the cache always holds the RAW disk bytes, and
//! `is_partial_view` is true exactly when the prompt showed less than all of
//! them — because of the 200-line cap, the 25 KB cap, or the runtime prompt
//! budget.

mod support;

use rebon_core::system_prompt::prime_injected_prompt_files;
use rebon_tools_core::FileStateCache;
use support::{boot, Booted, HomeGuard};

/// Prime a fresh cache exactly as the turn does.
fn prime(booted: &Booted, cwd: &str) -> FileStateCache {
    let cache = FileStateCache::new();
    prime_injected_prompt_files(&cache, cwd, Some(booted.prompt_seat().as_ref()));
    cache
}

#[test]
fn an_untruncated_memory_md_is_primed_without_the_partial_flag() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("small-memory");
    let body = "- [a](a.md) \u{2014} hook\n- [b](b.md) \u{2014} hook\n";
    let path = guard.seed_memory_md(&cwd, body);

    let cache = prime(&booted, &cwd);

    let state = cache.get(&path).expect("cache entry for MEMORY.md");
    assert_eq!(state.content, body, "raw disk bytes stored in cache");
    assert!(
        !state.is_partial_view,
        "under-budget untruncated MEMORY.md must not be marked partial"
    );
}

#[test]
fn a_budget_omitted_memory_md_is_primed_as_partial() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("budget-omitted-memory");
    let body = (0..80)
        .map(|i| {
            format!(
                "- [entry {i}](entry-{i}.md) \u{2014} {}",
                "detail".repeat(10)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = rebon_plugin_memory::memory::prompt::truncate_entrypoint(&body);
    assert!(
        !truncated.was_line_truncated && !truncated.was_byte_truncated,
        "test body must avoid line/byte truncation"
    );
    assert!(
        rebon_plugin_memory::memory::prompt::entrypoint_content_is_budgeted_partial(
            &truncated.content,
            rebon_plugin_memory::memory::prompt::MemoryPromptBudget::default_runtime(),
        ),
        "test body must exceed the runtime prompt budget"
    );
    let path = guard.seed_memory_md(&cwd, &body);

    let cache = prime(&booted, &cwd);

    let state = cache.get(&path).expect("cache entry for MEMORY.md");
    assert_eq!(state.content, body, "raw disk bytes stored in cache");
    assert!(
        state.is_partial_view,
        "runtime-budget-omitted MEMORY.md must force Read before Edit/Write"
    );
    assert_eq!(state.offset, None);
    assert_eq!(state.limit, None);
}

#[test]
fn a_line_truncated_memory_md_is_primed_as_partial() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("line-truncated-memory");
    // 201 entries → over the 200-line cap.
    let body = (0..=200)
        .map(|i| format!("- [entry {i}](e{i}.md)"))
        .collect::<Vec<_>>()
        .join("\n");
    let path = guard.seed_memory_md(&cwd, &body);

    let cache = prime(&booted, &cwd);

    let state = cache.get(&path).expect("cache entry for MEMORY.md");
    assert_eq!(
        state.content, body,
        "cache stores RAW disk bytes (for change diffing)"
    );
    assert!(
        state.is_partial_view,
        "line-truncated MEMORY.md must be marked partial so Edit/Write force a real Read first"
    );
}

#[test]
fn a_byte_truncated_memory_md_is_primed_as_partial() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("byte-truncated-memory");
    // 30 lines, each ~1.2 KB → ~36 KB: over the 25 KB byte cap, under the
    // 200-line cap.
    let long = format!("- [{}]({}.md)", "x".repeat(1000), "y".repeat(200));
    let body = std::iter::repeat(long.as_str())
        .take(30)
        .collect::<Vec<_>>()
        .join("\n");
    let path = guard.seed_memory_md(&cwd, &body);

    let cache = prime(&booted, &cwd);

    let state = cache.get(&path).expect("cache entry for MEMORY.md");
    assert_eq!(state.content, body, "cache stores RAW disk bytes");
    assert!(
        state.is_partial_view,
        "byte-truncated MEMORY.md must be marked partial"
    );
}

/// The per-project switch gates priming as well as the prompt section: what
/// the model was never shown must not be pre-seeded as "already read".
#[test]
fn the_auto_memory_switch_skips_memory_md_and_keeps_rebon_md() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("disabled-keeps-rebon");
    let rebon_path = guard.seed_project_rebon_md(&cwd, "# Project instructions\n");
    let memory_path = guard.seed_memory_md(&cwd, "- [a](a.md)\n");
    std::env::set_var("REBON_DISABLE_AUTO_MEMORY", "1");

    let cache = prime(&booted, &cwd);

    assert!(cache.has(&rebon_path), "REBON.md should still be primed");
    assert!(
        !cache.has(&memory_path),
        "MEMORY.md must not be primed when auto-memory is off"
    );
}

/// With the plugin off the seat reports nothing, so the memory entrypoint is
/// not primed — while the instruction files, which the engine primes itself,
/// still are.
#[test]
fn switching_the_plugin_off_leaves_only_the_instruction_files_primed() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("plugin-off-priming");
    let rebon_path = guard.seed_project_rebon_md(&cwd, "# Project instructions\n");
    let memory_path = guard.seed_memory_md(&cwd, "- [a](a.md)\n");

    let cache = prime(&booted, &cwd);
    assert!(cache.has(&rebon_path) && cache.has(&memory_path));

    booted.set_enabled(false);

    let cache = prime(&booted, &cwd);
    assert!(cache.has(&rebon_path), "REBON.md is the engine's own");
    assert!(!cache.has(&memory_path), "the memory entrypoint is gone");
}

/// All three files a session loads: global and project instructions from the
/// engine, the memory entrypoint from the seat.
#[test]
fn all_three_files_are_primed_when_all_are_present() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("all-three");
    let config_home = guard.home.join(".rebon");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(config_home.join("REBON.md"), "# global\n").unwrap();
    let global_path = std::fs::canonicalize(config_home.join("REBON.md")).unwrap();
    let project_path = guard.seed_project_rebon_md(&cwd, "# project\n");
    let memory_path = guard.seed_memory_md(&cwd, "- [a](a.md)\n");

    let cache = prime(&booted, &cwd);

    assert!(cache.has(&global_path), "global REBON.md primed");
    assert!(cache.has(&project_path), "project REBON.md primed");
    assert!(cache.has(&memory_path), "MEMORY.md primed");
}
