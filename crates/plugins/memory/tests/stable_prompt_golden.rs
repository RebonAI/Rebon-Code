//! The stable runtime-context plane with this plugin loaded is byte-identical
//! to the plane the engine assembled on its own before the auto-memory
//! section moved here, and losing exactly that block is what switching the
//! plugin off does.
//!
//! There is no frozen text fixture, because the section quotes absolute paths
//! under the user's config home — a golden file would pin one machine's
//! directories. The reference is the pre-move *rule* instead, copied verbatim
//! into [`legacy_memory_prompt`] from `rebon-core`'s
//! `resolve_dynamic_context_with_language` as it stood before the section moved
//! out of the engine. The plane is then compared three ways: the text
//! the seat contributes matches what that rule produces, the assembled plane
//! with the plugin on is the plane without it plus that block and nothing
//! else, and the block sits between the project instruction files and the MCP
//! instructions — rank 40, where the engine's own `memory` section stood.

mod support;

use std::path::Path;

use rebon_core::prompt_seat::{sections_for, PromptSubject, Rung};
use rebon_core::system_prompt::{
    build_stable_runtime_context_block, resolve_dynamic_context_with_language, SystemPromptConfig,
};
use support::{boot, Booted, HomeGuard};

// ---------------------------------------------------------------------------
// The pre-move rule, copied verbatim
// ---------------------------------------------------------------------------

/// `rebon-core`'s auto-memory resolution as it stood before the move, with
/// `available_tool_names` / `coordinator_mode` / `cwd` as the parameters the
/// engine held as locals. Not called by production code — it is the reference
/// the migrated path is compared against.
fn legacy_memory_prompt(cwd: &str, coordinator_mode: bool, available: &[&str]) -> Option<String> {
    let has_available_tool = |name: &str| available.iter().any(|tool| *tool == name);
    let can_save_memory = has_available_tool("SaveMemory");
    let can_write_memory =
        !coordinator_mode && (has_available_tool("Write") || has_available_tool("Edit"));
    if rebon_plugin_memory::memory::settings::is_auto_memory_enabled(Path::new(cwd)) {
        rebon_plugin_memory::memory::prompt::load_memory_prompt_with_options(
            cwd,
            rebon_plugin_memory::memory::prompt::MemoryPromptOptions {
                is_coordinator: coordinator_mode,
                can_write_memory,
                can_save_memory,
                budget: Some(
                    rebon_plugin_memory::memory::prompt::MemoryPromptBudget::default_runtime(),
                ),
            },
        )
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Assembling the plane both ways
// ---------------------------------------------------------------------------

/// A project with a `REBON.md` and a seeded `MEMORY.md`, so the plane carries
/// both a rank-30 and a rank-40 block.
fn project(guard: &HomeGuard, label: &str) -> String {
    let cwd = guard.cwd(label);
    guard.seed_project_rebon_md(&cwd, "# Project rules\nBe terse.\n");
    guard.seed_memory_md(&cwd, "- [a note](note.md) \u{2014} a hook\n");
    cwd
}

fn config(tools: &[&str]) -> SystemPromptConfig {
    SystemPromptConfig {
        model: "claude-opus-5".into(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        tool_names: tools.iter().map(|t| (*t).to_string()).collect(),
        deferred_tool_names: Vec::new(),
        platform: "linux".into(),
        shell: "zsh".into(),
        os_version: "Ubuntu 24.04".into(),
        language: None,
        auto_continue_background_agents: true,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
    }
}

/// The stable plane for `cwd`, with whatever the seat under `kernel`
/// contributes — the same two calls the executor makes.
fn stable_for(booted: &Booted, cwd: &str, coordinator: bool, tools: &[&str]) -> Option<String> {
    let config = config(tools);
    let mut ctx = resolve_dynamic_context_with_language(
        cwd,
        Some("sess-golden"),
        None,
        coordinator,
        false,
        None,
    );
    let session = booted.kernel.context().fork_scoped("session/golden");
    let subject = PromptSubject::new(&config.model)
        .with_session_id("sess-golden")
        .with_workspace(cwd, coordinator)
        .with_tools(config.tool_names.clone(), Vec::new());
    ctx.plugin_prompt_sections = sections_for(&session, &subject);
    build_stable_runtime_context_block(&config, &ctx)
}

/// The one section the plugin contributes, or `None`.
fn seat_memory_text(
    booted: &Booted,
    cwd: &str,
    coordinator: bool,
    tools: &[&str],
) -> Option<String> {
    let session = booted.kernel.context().fork_scoped("session/golden");
    let subject = PromptSubject::new("claude-opus-5")
        .with_workspace(cwd, coordinator)
        .with_tools(tools.iter().map(|t| (*t).to_string()).collect(), Vec::new());
    let sections = sections_for(&session, &subject);
    assert!(sections.len() <= 1, "one section per turn: {sections:?}");
    sections.into_iter().next().map(|section| {
        assert_eq!(section.rung, Rung::Memory);
        assert_eq!(section.name, "memory prompt");
        section.text
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The section the seat contributes is byte-for-byte what the engine's own
/// rule produced, across every combination that rule branched on.
#[test]
fn the_seat_section_matches_the_pre_move_rule_byte_for_byte() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = project(&guard, "rule-matrix");

    for (coordinator, tools) in [
        (false, vec!["Read", "Write", "SaveMemory"]),
        (false, vec!["Read", "Edit"]),
        (false, vec!["Read", "SaveMemory"]),
        (false, vec!["Read"]),
        (true, vec!["Agent", "Read", "SaveMemory"]),
        (true, vec!["Agent", "Read", "Write"]),
    ] {
        let expected = legacy_memory_prompt(&cwd, coordinator, &tools);
        let actual = seat_memory_text(&booted, &cwd, coordinator, &tools);
        assert_eq!(
            actual, expected,
            "coordinator={coordinator} tools={tools:?}"
        );
        assert!(actual.is_some(), "the matrix should exercise real text");
    }
}

/// The assembled plane with the plugin on is the plane without it plus the
/// memory block and nothing else, spliced in between the project instruction
/// files and the MCP instructions.
#[test]
fn the_plane_is_the_pluginless_plane_plus_the_memory_block() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = project(&guard, "splice");
    let tools = ["Read", "Write", "SaveMemory"];

    let with = stable_for(&booted, &cwd, false, &tools).expect("a stable plane");
    let memory = seat_memory_text(&booted, &cwd, false, &tools).expect("a memory block");

    booted.set_enabled(false);
    let without = stable_for(&booted, &cwd, false, &tools).expect("a stable plane");

    assert!(with.contains(&memory), "the block is in the plane");
    assert_eq!(
        with.replace(&format!("{memory}\n\n"), ""),
        without,
        "removing the block restores the pluginless plane exactly"
    );

    // Rank 40: after the rank-30 instruction files, before the rank-60 tool
    // guidance. (Nothing supplies MCP instructions in this fixture, so the
    // next engine section is the tool one.)
    let rules_at = with.find("# Project rules").expect("REBON.md rendered");
    let memory_at = with.find(&memory).expect("memory rendered");
    let tools_at = with.find("Rebon tools").or_else(|| with.find("# Tools"));
    assert!(rules_at < memory_at, "memory follows the instruction files");
    if let Some(tools_at) = tools_at {
        assert!(memory_at < tools_at, "memory precedes the tool guidance");
    }
}

/// The per-project switch still gates the section, and it gates it in the
/// plugin now: `autoMemoryEnabled=false` leaves the plane with no memory
/// block, exactly as the engine's `is_auto_memory_enabled` check did.
#[test]
fn the_auto_memory_switch_still_removes_the_block() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = project(&guard, "switched-off");
    let tools = ["Read", "Write", "SaveMemory"];

    assert!(seat_memory_text(&booted, &cwd, false, &tools).is_some());

    std::fs::create_dir_all(Path::new(&cwd).join(".rebon")).unwrap();
    std::fs::write(
        Path::new(&cwd).join(".rebon").join("settings.json"),
        r#"{"autoMemoryEnabled": false}"#,
    )
    .unwrap();

    assert_eq!(legacy_memory_prompt(&cwd, false, &tools), None);
    assert_eq!(seat_memory_text(&booted, &cwd, false, &tools), None);
    let plane = stable_for(&booted, &cwd, false, &tools).expect("a stable plane");
    assert!(
        plane.contains("# Project rules"),
        "the instruction files are not memory and stay"
    );
}

/// Switching the plugin off removes the memory block and leaves the project
/// instruction files, which are the hard constraint of the whole move.
#[test]
fn switching_the_plugin_off_keeps_the_instruction_files() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = project(&guard, "plugin-off");
    let tools = ["Read", "Write", "SaveMemory"];

    let memory = seat_memory_text(&booted, &cwd, false, &tools).expect("a memory block");
    booted.set_enabled(false);

    assert_eq!(seat_memory_text(&booted, &cwd, false, &tools), None);
    let plane = stable_for(&booted, &cwd, false, &tools).expect("a stable plane");
    assert!(!plane.contains(&memory), "the memory block is gone");
    assert!(
        plane.contains("# Project rules") && plane.contains("Be terse."),
        "REBON.md is still injected with the plugin off"
    );

    booted.set_enabled(true);
    assert_eq!(
        seat_memory_text(&booted, &cwd, false, &tools).as_deref(),
        Some(memory.as_str())
    );
}

/// The runtime prompt budget still caps the memory index and still shows up
/// in `/context` under its old name and category, now sourced from the seat.
#[test]
fn the_report_line_reflects_the_budgeted_memory_section() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = project(&guard, "budgeted-report");
    let dir = rebon_session::memory_paths::repo_memory_dir(&cwd).expect("memory dir");
    let body = (0..220)
        .map(|i| {
            format!(
                "- [memory entry {i}](entry-{i}.md) \u{2014} {}",
                "detail".repeat(20)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join("MEMORY.md"), &body).expect("MEMORY.md");
    let tools = ["Read", "Write"];

    let memory = seat_memory_text(&booted, &cwd, false, &tools).expect("a memory block");
    assert!(memory.contains("MEMORY.md index entries omitted by prompt budget"));
    assert!(!memory.contains("memory entry 219"));

    let unbudgeted = rebon_plugin_memory::memory::prompt::load_memory_prompt_with_options(
        &cwd,
        rebon_plugin_memory::memory::prompt::MemoryPromptOptions {
            is_coordinator: false,
            can_write_memory: true,
            can_save_memory: false,
            budget: None,
        },
    )
    .expect("unbudgeted memory prompt");

    let config = config(&tools);
    let mut ctx = resolve_dynamic_context_with_language(&cwd, None, None, false, false, None);
    let session = booted.kernel.context().fork_scoped("session/golden");
    ctx.plugin_prompt_sections = sections_for(
        &session,
        &PromptSubject::new(&config.model)
            .with_workspace(&cwd, false)
            .with_tools(config.tool_names.clone(), Vec::new()),
    );
    let report = rebon_core::system_prompt::build_system_prompt_report(&config, &ctx);
    let detail = report
        .sections
        .iter()
        .flat_map(|section| section.details.iter())
        .find(|detail| detail.name == "memory prompt")
        .expect("memory prompt detail");

    assert_eq!(detail.kind, "runtime_context_memory");
    assert!(detail.estimated_tokens < unbudgeted.len().div_ceil(4) as u64);
}

/// The memory budget is the memory section's alone: a large `REBON.md` is
/// injected whole beside a budget-trimmed memory index.
#[test]
fn the_instruction_files_are_not_budgeted_by_the_memory_cap() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = project(&guard, "rebon-md-not-budgeted");
    std::fs::write(
        Path::new(&cwd).join("REBON.md"),
        format!(
            "# Project instructions\n{}\nUNBUDGETED_REBON_SENTINEL\n",
            "follow this critical instruction.\n".repeat(400)
        ),
    )
    .expect("large REBON.md");
    let dir = rebon_session::memory_paths::repo_memory_dir(&cwd).expect("memory dir");
    let body = (0..220)
        .map(|i| {
            format!(
                "- [memory entry {i}](entry-{i}.md) \u{2014} {}",
                "detail".repeat(20)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join("MEMORY.md"), &body).expect("MEMORY.md");

    let plane = stable_for(&booted, &cwd, false, &["Read", "Write"]).expect("a stable plane");

    assert!(plane.contains("UNBUDGETED_REBON_SENTINEL"));
    assert!(plane.contains("follow this critical instruction"));
    assert!(plane.contains("MEMORY.md index entries omitted by prompt budget"));
    assert!(!plane.contains("memory entry 219"));
}

/// The files the section quotes come back off the seat, so the turn can seed
/// them into the read-state cache — and stop coming back with the plugin off.
#[test]
fn the_seat_reports_the_memory_entrypoints_it_quoted() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = project(&guard, "injected-files");

    let seat = booted.prompt_seat();
    let entrypoint = rebon_session::memory_paths::repo_memory_dir(&cwd)
        .expect("memory dir")
        .join("MEMORY.md");
    let files = seat.injected_files(&cwd);
    assert!(
        files.iter().any(|file| file.path == entrypoint),
        "the repo entrypoint is reported: {files:?}"
    );
    assert!(
        files.iter().all(|file| !file.is_partial_view),
        "a short MEMORY.md is not a partial view: {files:?}"
    );

    booted.set_enabled(false);
    assert!(seat.injected_files(&cwd).is_empty());
}
