//! The fixed per-request overhead the harness itself writes for the default
//! TUI main agent stays within budget.
//!
//! Counted: the system prompt, every eager tool definition (name,
//! description, JSON schema), and the harness-written runtime guidance in the
//! first message (environment, tool and session guidance, memory
//! instructions, scratchpad, retention reminder, the reminder wrapper), plus
//! the transient git-status header. Not counted, and absent here: user
//! content — REBON.md, MEMORY.md entries, skill listings, the git status
//! body. The rendering is the one the executor builds, from the same plugin
//! table, with an empty project and an empty config home.
//!
//! Its own test binary: the process kernel boots once and reads the config
//! home while it does.

use std::sync::Arc;

use rebon_core::prompt_seat::{sections_for, PromptSubject};
use rebon_core::query::filtered_eager_tools_from_engine;
use rebon_core::system_prompt::{
    build_split_system_prompt_for_profile, collect_git_status_for_profile,
    resolve_dynamic_context_with_language, PromptProfile, SystemPromptConfig,
};
use rebon_core::Engine;

/// The lean surface measured 21,036 characters (about 5.3k tokens at four
/// characters a token) when this budget was set; the margin absorbs the
/// machine-dependent paths in the memory and scratchpad lines. A rise past it
/// is a prompt change that has to justify itself.
const BUDGET_CHARS: usize = 22_000;

/// `<system-reminder>\n<runtime_context>\n` … `\n</runtime_context>\n</system-reminder>`.
const RUNTIME_WRAPPER_CHARS: usize = 74;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_tui_main_agent_overhead_stays_within_budget() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let project = tempfile::tempdir().expect("project dir");
    std::env::set_var("REBON_CONFIG_DIR", config_dir.path());
    let cwd = project.path().to_string_lossy().into_owned();

    // What the TUI does before its first listing.
    let kernel = rebon_harness::kernel_bootstrap::process_kernel();
    rebon_tool::set_sub_agents_enabled(true);
    rebon_tool::set_agent_registry_selection(
        Arc::new(rebon_tool::AgentRegistry::builtins_only()),
        false,
    );
    let engine = Engine::with_builtin_tools();
    engine.attach_upstream_tool_context(kernel.context().clone());
    let filter = rebon_harness::build_default_tool_filter_for_context(false, false);

    let profile = PromptProfile::Lean;
    let tools = filtered_eager_tools_from_engine(&engine, &filter);
    let tool_names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    let deferred = engine.filtered_deferred_tool_names(&filter);
    let config = SystemPromptConfig {
        model: "claude-opus-4-5".into(),
        model_marketing_name: Some("Claude Opus 4.5".into()),
        knowledge_cutoff: Some("May 2025".into()),
        tool_names: tool_names.clone(),
        deferred_tool_names: deferred.clone(),
        platform: "macos".into(),
        shell: "zsh".into(),
        os_version: "Darwin 27.0.0".into(),
        language: None,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    };
    let mut ctx = resolve_dynamic_context_with_language(
        &cwd,
        Some("budget-session"),
        None,
        false,
        false,
        None,
    );
    assert!(ctx.rebon_md_content.is_none(), "user content stays out");
    let session = kernel.context().fork_scoped("session/budget-session");
    ctx.plugin_prompt_sections = sections_for(
        &session,
        &PromptSubject::new(config.model.clone())
            .with_session_id("budget-session")
            .with_workspace(&cwd, false)
            .with_tools(tool_names.clone(), deferred.clone()),
    );
    let parts = build_split_system_prompt_for_profile(&config, &ctx, profile);

    let mut pieces: Vec<(String, usize)> = vec![("system".into(), parts.base_system.len())];
    let mut tool_rows: Vec<(String, usize)> = tools
        .iter()
        .map(|tool| {
            let json = serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            });
            (format!("tool {}", tool.name), json.to_string().len())
        })
        .collect();
    tool_rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    pieces.extend(tool_rows);
    let runtime = parts.runtime_context.unwrap_or_default();
    for section in runtime.split("\n\n") {
        let head: String = section
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(48)
            .collect();
        pieces.push((format!("runtime: {head}"), section.len() + 2));
    }
    pieces.push(("runtime wrapper".into(), RUNTIME_WRAPPER_CHARS));
    // The transient git-status header (the status body is user content).
    if let Some(status) = collect_git_status_for_profile(env!("CARGO_MANIFEST_DIR"), profile) {
        let header = status.split("\n\n").next().unwrap_or_default();
        pieces.push((
            "transient: gitStatus header".into(),
            "gitStatus: ".len() + header.len(),
        ));
    }

    let total: usize = pieces.iter().map(|(_, chars)| chars).sum();
    let breakdown = pieces
        .iter()
        .map(|(name, chars)| format!("{chars:>7}  ~{:>5} tok  {name}", chars / 4))
        .collect::<Vec<_>>()
        .join("\n");
    println!("{breakdown}\n{total:>7}  ~{:>5} tok  total", total / 4);
    assert!(
        total <= BUDGET_CHARS,
        "harness-authored overhead is {total} chars (~{} tokens), over the {BUDGET_CHARS}-char budget:\n{breakdown}",
        total / 4
    );

    // The exposure decisions the budget depends on.
    for eager in [
        "Read",
        "Edit",
        "Write",
        "Bash",
        "Agent",
        "ExitPlanMode",
        "ToolSearch",
    ] {
        assert!(
            tool_names.iter().any(|name| name == eager),
            "{eager} is eager"
        );
    }
    for name in ["EnterPlanMode", "MultiEdit"] {
        assert!(!tool_names.iter().any(|n| n == name), "{name} is not eager");
        assert!(deferred.iter().any(|n| n == name), "{name} is deferred");
    }
    assert!(!tool_names.iter().any(|name| name == "PlanLedger"));
    assert!(!deferred.iter().any(|name| name == "PlanLedger"));
    assert!(!tool_names.iter().any(|name| name == "RunWorkflow"));
}
