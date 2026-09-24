//! System prompt builder.
//!
//! Constructs the multi-section system prompt that gives the model its
//! identity, behavioural framing, tool-usage guidance, and environment
//! context.
//!
//! ## Design
//!
//! [`SystemPromptConfig`] captures the *static* information that does
//! not change between turns (model name, tool names, platform, shell,
//! OS version). The [`build_system_prompt`] function combines that with
//! *dynamic* per-turn state (cwd, is_git, REBON.md content, language)
//! to produce the final prompt string. This supports the lazy-env
//! collection strategy: static config is assembled once in wiring,
//! dynamic info is resolved at prompt time.

mod assembly;
mod sections;

pub use self::assembly::{
    coordinator_simple_mode_enabled, PromptAssembly, PromptPlane, PromptVariant,
};
pub use self::sections::sub_agent_notes_section;
#[cfg(test)]
use self::sections::{doing_tasks_section, using_tools_section};
use self::sections::{
    env_info_section, language_section, runtime_tools_section, scratchpad_section,
    session_specific_guidance_section,
};
pub use crate::prompt_seat::PluginPromptSection;

use std::path::PathBuf;

use rebon_tools_core::{file_mtime_ms, FileState, FileStateCache};

use crate::context_accounting::{
    prompt_text_section_report, PromptCostReport, PromptSectionReport,
};

/// Static configuration assembled once at wiring time.
#[derive(Debug, Clone)]
pub struct SystemPromptConfig {
    /// Model identifier (e.g. `"claude-opus-4-6"`).
    pub model: String,
    /// Marketing name shown to the model (e.g. `"Claude Opus 4.6"`).
    pub model_marketing_name: Option<String>,
    /// Knowledge cutoff string (e.g. `"May 2025"`).
    pub knowledge_cutoff: Option<String>,
    /// Names of all eager (non-deferred) tools registered on the engine.
    pub tool_names: Vec<String>,
    /// Names of deferred tools discoverable via ToolSearchTool.
    pub deferred_tool_names: Vec<String>,
    /// OS platform (e.g. `"win32"`, `"linux"`, `"darwin"`).
    pub platform: String,
    /// Shell name (e.g. `"bash"`, `"zsh"`).
    pub shell: String,
    /// OS version string (e.g. `"Windows 10 Enterprise LTSC 2021 10.0.19044"`).
    pub os_version: String,
    /// Explicit user response-language preference. `None` preserves the default prompt.
    pub language: Option<String>,
    /// User override for the Normal capability base system prompt.
    pub normal_system_prompt_override: Option<String>,
    /// User override for the Minimal capability base system prompt.
    pub minimal_system_prompt_override: Option<String>,
    /// User override for the Chat capability base system prompt.
    pub chat_system_prompt_override: Option<String>,
    /// Whether this frontend automatically starts a new turn when a
    /// background agent completes while the session is idle. The TUI's
    /// resident event loop does; ACP and headless frontends have no such
    /// loop — there, completion notifications are injected at the start
    /// of the next user-initiated turn. Gates the wording of the
    /// background-agent guidance bullet so the prompt never promises a
    /// wake-up the frontend cannot deliver.
    pub auto_continue_background_agents: bool,
}

/// Dynamic per-turn context resolved lazily in `execute()`.
#[derive(Debug, Clone, Default)]
pub struct DynamicPromptContext {
    /// Current working directory.
    pub cwd: String,
    /// Whether the cwd is inside a git repo.
    pub is_git: bool,
    /// Optional multi-line git-status summary (branch, main branch,
    /// short status, recent commits, user name), in the shape
    /// [`collect_git_status`] produces.
    /// Populated by
    /// [`collect_git_status`] when the cwd is inside a repo.
    pub git_status: Option<String>,
    /// Optional language preference (e.g. `"Chinese"`).
    pub language: Option<String>,
    /// Content of REBON.md files, if loaded.
    pub rebon_md_content: Option<String>,
    /// MCP server instructions, if any.
    pub mcp_instructions: Option<String>,
    /// Today's date string for the session (e.g. `"2026-04-10"`).
    pub session_date: Option<String>,
    /// Whether we're in coordinator mode.
    pub coordinator_mode: bool,
    /// Whether coordinator implementation workers should be isolated in git worktrees.
    pub coordinator_use_worktree: bool,
    /// Worker tools context (injected when coordinator mode is on).
    pub worker_tools_context: Option<String>,
    /// Scratchpad directory injected into coordinator worker context.
    pub scratchpad_dir: Option<String>,
    /// Plugin-contributed prompt sections: the turn's snapshot of the
    /// kernel's `prompt-sections` seat ([`crate::prompt_seat`]), filled by
    /// the executor once it knows the model and the tool projection the
    /// turn speaks with. Empty when nothing contributes — assembly is then
    /// byte-identical to the pre-seat prompt.
    pub plugin_prompt_sections: Vec<PluginPromptSection>,
}

/// Split prompt parts for feature-flagged stable base-system requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptParts {
    /// Stable top-level system prompt.
    pub base_system: String,
    /// Low-churn runtime context that can sit near the front of the
    /// provider-visible prompt for prefix-cache reuse.
    pub runtime_context: Option<String>,
    /// High-churn per-request context that should be appended to the
    /// current user turn and kept out of durable history.
    pub transient_context: Option<String>,
}

const MINIMAL_SYSTEM_PROMPT: &str = "Act as the user's software engineering agent and follow their directions. Read the relevant REBON.md instructions before changing files. Start with Bash for shell operations and Read for file access. Consult ToolSearch only if you need a different tool; it returns that tool's complete schema. Obtain explicit approval before any destructive action or action affecting shared state. Keep secrets safe. Check your changes and give an honest account of any failures.";

/// Persona for Chat capability sessions.
///
/// States the absence of tools outright. A model given no tools but a persona
/// written for one narrates the work instead of doing it — "I've updated the
/// file" with nothing updated — which reads as a broken agent rather than a
/// deliberately toolless conversation.
const CHAT_SYSTEM_PROMPT: &str = "Be a helpful conversational assistant. No tools are available to you in this session. You cannot access or change files, execute commands, or browse the web; your words cannot alter the user's machine. Base answers on your own knowledge and the information the user supplies. If a request entails action on their system, explain what you would do and leave execution to them. Never imply that you performed an action you have not taken. Acknowledge uncertainty and any answer that relies on information you cannot observe.";

/// Build the split prompt used by Chat capability sessions.
///
/// No runtime or transient context: a session with no tools has no workspace
/// to describe, and the environment block would be inert text in every turn's
/// cache prefix.
pub fn build_chat_prompt_parts(prompt_override: Option<&str>) -> PromptParts {
    PromptParts {
        base_system: non_empty_prompt_override(prompt_override)
            .unwrap_or(CHAT_SYSTEM_PROMPT)
            .to_string(),
        runtime_context: None,
        transient_context: None,
    }
}

/// Build the split prompt used by Minimal capability sessions.
pub fn build_minimal_prompt_parts(prompt_override: Option<&str>) -> PromptParts {
    build_minimal_prompt_parts_for_profile(prompt_override, false)
}

/// Build the split Minimal prompt, optionally on the anchored profile.
///
/// Rebon's own Minimal persona names `Bash`/`Read`/`ToolSearch` because those
/// are the tools that session will ever see. An anchored session sees the
/// upstream Minimal pair instead, and upstream's measurement depends on that
/// preset's persona being reproduced exactly — including the fact that it says
/// nothing about tools at all.
pub fn build_minimal_prompt_parts_for_profile(
    prompt_override: Option<&str>,
    anchored: bool,
) -> PromptParts {
    let default = if anchored {
        crate::anchored_minimal::ANCHORED_MINIMAL_PERSONA
    } else {
        MINIMAL_SYSTEM_PROMPT
    };
    PromptParts {
        base_system: non_empty_prompt_override(prompt_override)
            .unwrap_or(default)
            .to_string(),
        runtime_context: None,
        transient_context: None,
    }
}

/// Build the complete prompt used by Minimal capability sessions.
pub fn build_minimal_system_prompt(prompt_override: Option<&str>) -> String {
    non_empty_prompt_override(prompt_override)
        .unwrap_or(MINIMAL_SYSTEM_PROMPT)
        .to_string()
}

/// Build the full system prompt from static config + dynamic context.
///
/// The parts of [`build_split_system_prompt`] are joined by double
/// newlines.
pub fn build_system_prompt(config: &SystemPromptConfig, ctx: &DynamicPromptContext) -> String {
    let parts = build_split_system_prompt(config, ctx);
    join_prompt_parts(
        parts.base_system,
        parts.runtime_context,
        parts.transient_context,
    )
}

/// Build a section-level estimated-token report for the system prompt without
/// changing or appending to the prompt text sent to the provider.
pub fn build_system_prompt_report(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
) -> PromptCostReport {
    let parts = build_split_system_prompt(config, ctx);
    let mut sections = vec![prompt_text_section_report(
        "base system prompt",
        "system_prompt_base",
        &parts.base_system,
    )];

    if let Some(runtime_context) = parts.runtime_context.as_deref().filter(|s| !s.is_empty()) {
        sections.push(stable_runtime_context_report(config, ctx, runtime_context));
    }
    if let Some(transient_context) = parts.transient_context.as_deref().filter(|s| !s.is_empty()) {
        sections.push(transient_runtime_context_report(
            config,
            ctx,
            transient_context,
        ));
    }

    PromptCostReport::new(sections)
}

fn stable_runtime_context_report(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
    runtime_context: &str,
) -> PromptSectionReport {
    let mut details = Vec::new();
    details.push(prompt_text_section_report(
        "environment context",
        "runtime_context",
        &env_info_section(config, ctx),
    ));
    if let Some(ref lang) = ctx.language {
        details.push(prompt_text_section_report(
            "language preference",
            "runtime_context",
            &language_section(lang),
        ));
    }
    if let Some(content) = optional_non_empty_section(&ctx.rebon_md_content) {
        details.push(prompt_text_section_report(
            "REBON.md instructions",
            "runtime_context",
            &content,
        ));
    }
    details.extend(plugin_section_details(
        ctx,
        crate::prompt_seat::Rung::Memory,
    ));
    if let Some(mcp) = optional_non_empty_section(&ctx.mcp_instructions) {
        details.push(prompt_text_section_report(
            "MCP instructions",
            "runtime_context",
            &mcp,
        ));
    }
    if !config.tool_names.is_empty() || !config.deferred_tool_names.is_empty() {
        let runtime_tools = runtime_tools_section(&config.tool_names, &config.deferred_tool_names);
        let tool_details =
            config
                .tool_names
                .iter()
                .map(|name| {
                    prompt_text_section_report(name.clone(), "provider_visible_tool_name", name)
                })
                .chain(config.deferred_tool_names.iter().map(|name| {
                    prompt_text_section_report(name.clone(), "deferred_tool_name", name)
                }))
                .collect::<Vec<_>>();
        details.push(
            prompt_text_section_report("tool guidance", "runtime_tool_context", &runtime_tools)
                .with_details(tool_details),
        );
    }
    if let Some(section) = session_specific_guidance_section(
        &config.tool_names,
        &config.deferred_tool_names,
        config.auto_continue_background_agents,
    ) {
        details.push(prompt_text_section_report(
            "skill guidance",
            "runtime_skill_guidance",
            &section,
        ));
    }
    details.extend(plugin_section_details(
        ctx,
        crate::prompt_seat::Rung::Context,
    ));
    if let Some(ref dir) = ctx.scratchpad_dir {
        if !dir.is_empty() {
            details.push(prompt_text_section_report(
                "scratchpad directory",
                "runtime_context",
                &scratchpad_section(dir),
            ));
        }
    }
    details.push(prompt_text_section_report(
        "tool result retention reminder",
        "runtime_context",
        TOOL_RESULT_RETENTION_REMINDER,
    ));

    prompt_text_section_report("runtime context", "runtime_context", runtime_context)
        .with_details(details)
}

/// The turn's plugin sections standing on `rung`, as report details in render
/// order — so `/context` accounts for a plugin's contribution wherever the
/// plugin put it, and each detail is inserted at the rank it renders at.
///
/// Naming: the detail takes the section's own name, and the category comes
/// from the rung, because that is what tells a reader which *kind* of content
/// this is regardless of which plugin supplied it.
fn plugin_section_details(
    ctx: &DynamicPromptContext,
    rung: crate::prompt_seat::Rung,
) -> Vec<PromptSectionReport> {
    let mut sections: Vec<PluginPromptSection> = ctx
        .plugin_prompt_sections
        .iter()
        .filter(|section| section.rung == rung && !section.text.is_empty())
        .cloned()
        .collect();
    crate::prompt_seat::sort_sections(&mut sections);
    let kind = match rung {
        crate::prompt_seat::Rung::Memory => "runtime_context_memory",
        _ => "runtime_context_plugin",
    };
    sections
        .into_iter()
        .map(|section| prompt_text_section_report(section.name, kind, &section.text))
        .collect()
}

fn transient_runtime_context_report(
    _config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
    transient_context: &str,
) -> PromptSectionReport {
    let mut details = Vec::new();
    if let Some(worker_ctx) = optional_non_empty_section(&ctx.worker_tools_context) {
        details.push(prompt_text_section_report(
            "worker tools context",
            "transient_context",
            &worker_ctx,
        ));
    }
    if let Some(git) = optional_non_empty_section(&ctx.git_status) {
        let section = format!("gitStatus: {git}");
        details.push(prompt_text_section_report(
            "git status",
            "transient_context",
            &section,
        ));
    }

    prompt_text_section_report("transient context", "transient_context", transient_context)
        .with_details(details)
}

/// Build only the stable base prompt for top-level provider `system` /
/// `instructions` fields.
pub fn build_base_system_prompt(config: &SystemPromptConfig) -> String {
    if let Some(prompt_override) =
        non_empty_prompt_override(config.normal_system_prompt_override.as_deref())
    {
        return prompt_override.to_string();
    }

    PromptAssembly::for_variant(&PromptVariant::Standard)
        .assemble_base(config, &DynamicPromptContext::default())
}

fn non_empty_prompt_override(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

fn optional_non_empty_section(value: &Option<String>) -> Option<String> {
    value.as_ref().filter(|value| !value.is_empty()).cloned()
}

fn finish_sections(sections: Vec<String>) -> Option<String> {
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

const TOOL_RESULT_RETENTION_REMINDER: &str =
    "As you use tool results, record in your response any important \
         details you could need again: the original result might later \
         be cleared.";

/// Build low-churn runtime context.
pub fn build_stable_runtime_context_block(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
) -> Option<String> {
    PromptAssembly::for_variant(&PromptVariant::Standard).assemble_stable(config, ctx)
}

/// Build high-churn per-request runtime context.
///
/// Providers without request-scoped transient context may materialize this
/// block into durable history and deduplicate it with exact string equality.
/// Keep output deterministic for the same logical state: do not add
/// timestamps, process IDs, random IDs, or unordered map iteration here.
pub fn build_transient_runtime_context_block(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
) -> Option<String> {
    PromptAssembly::for_variant(&PromptVariant::Standard).assemble_transient(config, ctx)
}

/// Build dynamic per-turn runtime context.
pub fn build_runtime_context_block(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
) -> Option<String> {
    finish_sections(
        [
            build_stable_runtime_context_block(config, ctx),
            build_transient_runtime_context_block(config, ctx),
        ]
        .into_iter()
        .flatten()
        .collect(),
    )
}

/// Build split prompt parts. The variant hook decides the base plane: in
/// coordinator mode the stable base is the coordinator contract; otherwise
/// it is the standard main-agent prompt.
pub fn build_split_system_prompt(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
) -> PromptParts {
    let variant = PromptVariant::from_context(ctx);
    let assembly = PromptAssembly::for_variant(&variant);
    // The user's Normal base override replaces the standard persona and
    // nothing else. The coordinator variant's base plane is its worker-routing
    // contract rather than a persona — before the assembly registry existed
    // that path returned early, never reading this setting, and replacing a
    // contract with a persona would disable the mechanism a coordinator is
    // for.
    let base_system = match (
        &variant,
        non_empty_prompt_override(config.normal_system_prompt_override.as_deref()),
    ) {
        (PromptVariant::Standard, Some(prompt_override)) => prompt_override.to_string(),
        _ => assembly.assemble_base(config, ctx),
    };
    PromptParts {
        base_system,
        runtime_context: assembly.assemble_stable(config, ctx),
        transient_context: assembly.assemble_transient(config, ctx),
    }
}

fn join_prompt_parts(
    base_system: String,
    runtime_context: Option<String>,
    transient_context: Option<String>,
) -> String {
    [Some(base_system), runtime_context, transient_context]
        .into_iter()
        .flatten()
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------
// Helpers for resolving dynamic context
// ---------------------------------------------------------------------------

/// Check if a directory is inside a git repository.
///
/// Runs `git rev-parse --is-inside-work-tree` and returns true if the
/// exit status is 0. Returns false on any error (not a git repo,
/// `git` not on PATH, etc.).
pub fn check_is_git(cwd: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Max characters of `git status --short` to inline before the trailing
/// lines are dropped.
const GIT_STATUS_MAX_CHARS: usize = 2000;

/// Assemble a multi-line git-status summary for the system prompt.
///
/// Returns
/// `None` if the cwd is not a repo, if `git` is not available, or if
/// any of the probed commands return a non-zero exit. Partial
/// failures fall through: e.g. a missing default-branch is rendered
/// as an empty line rather than dropping the whole block.
///
/// Output shape (single string, sections joined by `\n\n`):
///
/// ```text
/// The git status below was captured at the beginning of this turn. ...
///
/// Current branch: main
///
/// Main branch (normally the branch to use for PRs): main
///
/// Git user: Jane Dev
///
/// Status:
///  M rebon/Cargo.toml
///
/// Recent commits:
/// abc1234 last commit
/// ```
pub fn collect_git_status(cwd: &str) -> Option<String> {
    if !check_is_git(cwd) {
        return None;
    }

    let branch = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    // Main branch: prefer `origin/HEAD` symbolic ref, fall back to
    // common defaults. A cached default-branch lookup could be consulted
    // instead; this uses the two most common shapes.
    let main_branch = run_git(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .map(|s| {
        // `origin/main` → `main`
        s.strip_prefix("origin/").map(str::to_string).unwrap_or(s)
    })
    .or_else(|| {
        // Fall back to `init.defaultBranch` (config) or `main`/`master`.
        run_git(cwd, &["config", "--get", "init.defaultBranch"])
    })
    .unwrap_or_else(|| "main".to_string());

    let status_raw =
        run_git(cwd, &["--no-optional-locks", "status", "--short"]).unwrap_or_default();
    let status = if status_raw.chars().count() > GIT_STATUS_MAX_CHARS {
        let truncated: String = status_raw.chars().take(GIT_STATUS_MAX_CHARS).collect();
        format!(
            "{truncated}\n... (shortened because the output exceeds 2k characters. \
             For additional information, use BashTool to run \"git status\")"
        )
    } else {
        status_raw
    };

    let log =
        run_git(cwd, &["--no-optional-locks", "log", "--oneline", "-n", "5"]).unwrap_or_default();

    let user_name = run_git(cwd, &["config", "user.name"]);

    let mut lines: Vec<String> = Vec::new();
    // The snapshot is re-collected at the start of every user turn and a
    // fresh copy is appended to history (or attached transiently), so the
    // header must not claim conversation-lifetime validity: with several
    // snapshots in one transcript, the newest one wins.
    lines.push(
        "The git status below was captured at the beginning of this turn. \
         It stays fixed throughout the turn. A newer gitStatus snapshot later \
         in the conversation replaces this one as the current snapshot."
            .to_string(),
    );
    lines.push(format!("Current branch: {branch}"));
    lines.push(format!(
        "Main branch (normally the branch to use for PRs): {main_branch}"
    ));
    if let Some(name) = user_name.filter(|s| !s.is_empty()) {
        lines.push(format!("Git user: {name}"));
    }
    let status_body = if status.is_empty() {
        "(clean)".to_string()
    } else {
        status
    };
    lines.push(format!("Status:\n{status_body}"));
    lines.push(format!("Recent commits:\n{log}"));

    Some(lines.join("\n\n"))
}

/// Run a git subcommand under `cwd`, returning its trimmed stdout on
/// success. Returns `None` on non-zero exit, missing binary, or any
/// other I/O error — callers decide whether to substitute a default.
fn run_git(cwd: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_string())
}

/// Attempt to load REBON.md content from canonical instruction discovery.
///
/// Loads global instructions, upward project/dot-project instructions, rules,
/// cwd-local private instructions, and supported markdown includes in the order
/// returned by `rebon-instructions` discovery.
pub fn load_rebon_md(cwd: &str) -> Option<String> {
    let parts: Vec<String> = rebon_instructions::instruction_files::discover_instruction_files(cwd)
        .into_iter()
        .filter(|file| !file.content.trim().is_empty())
        .map(|file| {
            let kind = match file.r#type {
                rebon_instructions::memory_type::MemoryType::User => {
                    "global instructions from the user"
                }
                rebon_instructions::memory_type::MemoryType::Local => {
                    "instructions local/private to this workspace"
                }
                _ => "instructions for this project",
            };
            format!(
                "Instruction text from {} ({}):\n\n{}",
                file.path,
                kind,
                file.content.trim()
            )
        })
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// The scratchpad a session is told to use, or `None` for a session with
/// no id to key one by.
///
/// Every session gets one, not just a coordinator. The directory is keyed
/// by project and session, so two sessions in the same project do not
/// share a temp directory, and a session told nothing invents a place
/// instead — which in practice has meant the user's own config home.
fn scratchpad_dir_for_session(cwd: &str, session_id: Option<&str>) -> Option<String> {
    Some(scratchpad_dir_for(cwd, session_id?))
}

/// The path lives in `rebon-session` so the job store, which ends hosted
/// sessions and cannot depend on this crate, resolves the same one.
pub use rebon_session::{remove_scratchpad_for, scratchpad_dir_for};

/// Suffix appended to every sub-agent system prompt: the shared
/// sub-agent notes plus a scratchpad pointer, so workers put temp
/// artifacts (test scripts, packaging output, export bundles) outside
/// the user's project tree instead of scattering them into the cwd.
pub fn sub_agent_prompt_suffix(scratchpad_dir: &str) -> String {
    format!(
        "{notes}\n\n{scratchpad}",
        notes = sub_agent_notes_section(),
        scratchpad = sections::scratchpad_section(scratchpad_dir),
    )
}

/// Resolve the full [`DynamicPromptContext`] for a given cwd.
///
/// This is the "lazy" resolution point — called inside `execute()`
/// rather than at wiring time, so session startup is not blocked by
/// subprocess calls or file I/O.
pub fn resolve_dynamic_context_with_language(
    cwd: &str,
    session_id: Option<&str>,
    mcp_client: Option<&dyn rebon_tool::McpClient>,
    coordinator_mode: bool,
    coordinator_use_worktree: bool,
    language: Option<String>,
) -> DynamicPromptContext {
    let is_git = check_is_git(cwd);
    // Only shell out for the detailed git status when we already know
    // the cwd is a repo — saves 4 forked `git` invocations per turn on
    // non-repo sessions.
    let git_status = if is_git {
        collect_git_status(cwd)
    } else {
        None
    };
    let rebon_md_content = load_rebon_md(cwd);
    let scratchpad_dir = scratchpad_dir_for_session(cwd, session_id);

    let session_date = Some(crate::attachments::current_local_iso_date());

    let is_simple = rebon_types::env::env_truthy("REBON_SIMPLE");
    let mcp_server_names = mcp_client
        .map(|client| client.server_names())
        .unwrap_or_default();
    let mcp_server_name_refs = mcp_server_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let worker_tools_context = crate::coordinator_mode::coordinator_user_context(
        coordinator_mode,
        is_simple,
        &mcp_server_name_refs,
        scratchpad_dir.as_deref(),
    );

    DynamicPromptContext {
        cwd: cwd.to_string(),
        is_git,
        git_status,
        language,
        rebon_md_content,
        mcp_instructions: None,
        session_date,
        coordinator_mode,
        coordinator_use_worktree,
        worker_tools_context,
        scratchpad_dir,
        // The seat is asked per turn by the executor, which alone knows the
        // model and tool projection the prompt is for.
        plugin_prompt_sections: Vec::new(),
    }
}

/// Prime the shared [`FileStateCache`] with entries for every file the
/// prompt just put in front of the model verbatim.
///
/// Two sources, one rule. The engine's own is the project instruction
/// documents (the `rebon-md` section, rank 30), which are loaded whatever
/// plugins are on. The other is whatever the `prompt-sections` seat reports
/// through [`crate::prompt_seat::PromptSectionProvider::injected_files`] —
/// today the `memory` plugin's `MEMORY.md` entrypoints, and empty on a host
/// with no kernel or with that plugin switched off.
///
/// Implements the behaviour of two call sites:
/// * Every instruction file the startup scan found is seeded into the
///   cache, so Edit/Write can
///   enforce "must Read first" against them from turn one.
/// * The same for the per-tool-round
///   nested-memory attachments.
///
/// Semantics: the cached entry always
/// holds the RAW disk bytes, and `is_partial_view` says whether the prompt
/// showed less than all of them. REBON.md is injected unchanged by
/// [`load_rebon_md`] (no truncation, no HTML-comment strip, no frontmatter
/// strip). A seat-reported file may well be partial — the memory entrypoint
/// is capped and budgeted — and a partial view forces the model to run Read
/// before Edit/Write.
///
/// Intentionally idempotent: every call overwrites whatever was there
/// before. The engine drives one turn at a time with a fresh cache in
/// the default TUI wiring, so re-priming is a no-op relative to the
/// last turn. When a shared cache persists across turns (coordinator /
/// worker inheritance via `spec.tool_context.file_state_cache()`), the
/// overwrite keeps the cached timestamp / content aligned with disk
/// — which is exactly what we want when the system prompt re-injects
/// these files each turn.
///
/// Errors reading individual files are swallowed: a missing document means no
/// entry, which is strictly better than failing the whole turn. The
/// Edit/Write "must Read first" check will continue to trip the model if it
/// tries to edit a file we couldn't prime, which is the correct behaviour.
pub fn prime_injected_prompt_files(
    cache: &FileStateCache,
    cwd: &str,
    seat: Option<&crate::prompt_seat::PromptSeat>,
) {
    // --- Instruction files (raw disk bytes, partial when injected view was stripped) ---
    for file in rebon_instructions::instruction_files::discover_instruction_files(cwd) {
        let path_buf = PathBuf::from(&file.path);
        let path = std::fs::canonicalize(&path_buf).unwrap_or(path_buf);
        let timestamp_ms = file_mtime_ms(&path).unwrap_or(0);
        cache.set(
            &path,
            FileState {
                content: file.raw_content.unwrap_or(file.content),
                timestamp_ms,
                offset: None,
                limit: None,
                is_partial_view: file.content_differs_from_disk,
            },
        );
    }

    // --- Whatever the prompt seat's sections quoted verbatim ---
    let Some(seat) = seat else {
        return;
    };
    for injected in seat.injected_files(cwd) {
        let timestamp_ms = file_mtime_ms(&injected.path).unwrap_or(0);
        cache.set(
            &injected.path,
            FileState {
                content: injected.content,
                timestamp_ms,
                offset: None,
                limit: None,
                is_partial_view: injected.is_partial_view,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static SUB_AGENT_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn sample_config() -> SystemPromptConfig {
        SystemPromptConfig {
            model: "claude-opus-4-6".to_string(),
            model_marketing_name: Some("Claude Opus 4.6".to_string()),
            knowledge_cutoff: Some("May 2025".to_string()),
            tool_names: vec![
                "Bash".into(),
                "Read".into(),
                "Write".into(),
                "Edit".into(),
                "Glob".into(),
                "Grep".into(),
                "TodoWrite".into(),
            ],
            deferred_tool_names: Vec::new(),
            platform: "win32".to_string(),
            shell: "bash".to_string(),
            os_version: "Windows 10 Enterprise LTSC 2021 10.0.19044".to_string(),
            language: None,
            normal_system_prompt_override: None,
            minimal_system_prompt_override: None,
            chat_system_prompt_override: None,
            auto_continue_background_agents: true,
        }
    }

    fn sample_config_with_deferred_tool() -> SystemPromptConfig {
        let mut config = sample_config();
        config.tool_names.push("FakeEagerToolForCacheTest".into());
        config
            .deferred_tool_names
            .push("FakeDeferredToolForCacheTest".into());
        config
    }

    /// What the `memory` plugin puts on the prompt seat: one section on
    /// [`crate::prompt_seat::Rung::Memory`], rank 40 of the stable plane —
    /// the slot the engine's own `memory` section held.
    fn memory_section(text: &str) -> PluginPromptSection {
        PluginPromptSection::new("memory prompt", crate::prompt_seat::Rung::Memory, text)
    }

    fn sample_context() -> DynamicPromptContext {
        DynamicPromptContext {
            cwd: "/home/user/project".to_string(),
            is_git: true,
            git_status: None,
            language: None,
            rebon_md_content: None,
            mcp_instructions: None,
            session_date: Some("2026-04-10".to_string()),
            coordinator_mode: false,
            coordinator_use_worktree: false,
            worker_tools_context: None,
            scratchpad_dir: None,
            plugin_prompt_sections: Vec::new(),
        }
    }

    #[test]
    fn system_prompt_report_has_expected_section_names_and_preserves_prompt_output() {
        let config = sample_config_with_deferred_tool();
        let ctx = DynamicPromptContext {
            plugin_prompt_sections: vec![memory_section("remember this expensive project fact")],
            worker_tools_context: Some("worker context".to_string()),
            ..sample_context()
        };
        let before = build_system_prompt(&config, &ctx);

        let report = build_system_prompt_report(&config, &ctx);
        let after = build_system_prompt(&config, &ctx);

        assert_eq!(before, after);
        let names = report
            .sections
            .iter()
            .map(|section| section.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"base system prompt"));
        assert!(names.contains(&"runtime context"));
        assert!(names.contains(&"transient context"));
        assert!(report
            .sections
            .iter()
            .flat_map(|section| section.details.iter())
            .any(|detail| detail.name == "memory prompt"));
        assert!(report
            .sections
            .iter()
            .flat_map(|section| section.details.iter())
            .any(|detail| detail.name == "skill guidance"));
    }

    #[test]
    fn system_prompt_report_tool_guidance_lists_top_tool_names() {
        let mut config = sample_config();
        config.tool_names = vec!["Z".repeat(80), "Bash".into()];
        config.deferred_tool_names = vec!["DeferredHuge".repeat(20)];

        let report = build_system_prompt_report(&config, &sample_context());
        let tool_guidance = report
            .sections
            .iter()
            .flat_map(|section| section.details.iter())
            .find(|detail| detail.name == "tool guidance")
            .expect("tool guidance section report");

        assert_eq!(tool_guidance.top_details(1)[0].kind, "deferred_tool_name");
    }

    #[test]
    fn split_system_prompt_keeps_legacy_full_shape() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            git_status: Some("Current branch: main".to_string()),
            language: Some("Chinese".to_string()),
            rebon_md_content: Some("REBON project instructions".to_string()),
            plugin_prompt_sections: vec![memory_section("MEMORY project note")],
            mcp_instructions: Some("MCP server instruction".to_string()),
            scratchpad_dir: Some("/tmp/rebon-scratch".to_string()),
            ..sample_context()
        };

        let legacy = build_system_prompt(&config, &ctx);
        let parts = build_split_system_prompt(&config, &ctx);
        assert_eq!(
            legacy,
            join_prompt_parts(
                parts.base_system.clone(),
                parts.runtime_context.clone(),
                parts.transient_context.clone(),
            )
        );

        assert!(parts.base_system.contains("# System"));
        assert!(!parts.base_system.contains("/home/user/project"));
        assert!(!parts.base_system.contains("Current branch: main"));
        assert!(!parts.base_system.contains("REBON project instructions"));
        assert!(!parts.base_system.contains("MEMORY project note"));

        let runtime = parts.runtime_context.expect("runtime context");
        assert!(runtime.contains("# Environment"));
        assert!(runtime.contains("/home/user/project"));
        assert!(!runtime.contains("Current branch: main"));
        assert!(runtime.contains("Chinese"));
        assert!(runtime.contains("REBON project instructions"));
        assert!(runtime.contains("MEMORY project note"));
        assert!(runtime.contains("MCP server instruction"));
        assert!(runtime.contains("/tmp/rebon-scratch"));

        let transient = parts.transient_context.expect("transient context");
        assert!(transient.contains("Current branch: main"));
        assert!(!transient.contains("/tmp/rebon-scratch"));
    }

    #[test]
    fn coordinator_split_preserves_contract_and_moves_runtime_context() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            coordinator_mode: true,
            worker_tools_context: Some("Worker tool routing contract".to_string()),
            git_status: Some("Current branch: coordinator".to_string()),
            plugin_prompt_sections: vec![memory_section("Coordinator memory note")],
            ..sample_context()
        };

        let parts = build_split_system_prompt(&config, &ctx);
        assert!(parts.base_system.contains("coordinator"));
        assert!(!parts.base_system.contains("Worker tool routing contract"));
        assert!(!parts.base_system.contains("/home/user/project"));
        assert!(!parts.base_system.contains("Coordinator memory note"));

        let runtime = parts.runtime_context.expect("runtime context");
        assert!(!runtime.contains("Worker tool routing contract"));
        assert!(runtime.contains("/home/user/project"));
        assert!(!runtime.contains("Current branch: coordinator"));
        assert!(runtime.contains("Coordinator memory note"));

        let transient = parts.transient_context.expect("transient context");
        assert!(transient.contains("Worker tool routing contract"));
        assert!(transient.contains("Current branch: coordinator"));
    }

    #[test]
    fn build_system_prompt_returns_non_empty() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(!prompt.is_empty());
    }

    #[test]
    fn normal_prompt_override_replaces_only_the_base_persona() {
        let mut config = sample_config_with_deferred_tool();
        config.normal_system_prompt_override = Some("  custom normal persona  ".to_string());
        let ctx = DynamicPromptContext {
            rebon_md_content: Some("project instructions".to_string()),
            plugin_prompt_sections: vec![memory_section("memory instructions")],
            ..sample_context()
        };

        let parts = build_split_system_prompt(&config, &ctx);

        assert_eq!(parts.base_system, "  custom normal persona  ");
        let runtime = parts.runtime_context.expect("runtime context");
        assert!(runtime.contains("/home/user/project"));
        assert!(runtime.contains("project instructions"));
        assert!(runtime.contains("memory instructions"));
        assert!(runtime.contains("FakeEagerToolForCacheTest"));
        assert!(runtime.contains("FakeDeferredToolForCacheTest"));
    }

    /// The override replaces the whole base plane, plugin sections on base
    /// rungs included: a persona the user wrote is not decorated by
    /// plugins. The runtime planes still take theirs.
    #[test]
    fn normal_prompt_override_replaces_plugin_base_sections_too() {
        use crate::prompt_seat::{PluginPromptSection, Rung};
        let mut config = sample_config();
        config.normal_system_prompt_override = Some("custom normal persona".to_string());
        let ctx = DynamicPromptContext {
            plugin_prompt_sections: vec![
                PluginPromptSection::new("style", Rung::Style, "STYLE-PLUGIN"),
                PluginPromptSection::new("ctx", Rung::Context, "CONTEXT-PLUGIN"),
            ],
            ..sample_context()
        };

        let parts = build_split_system_prompt(&config, &ctx);
        assert_eq!(parts.base_system, "custom normal persona");
        assert!(parts
            .runtime_context
            .expect("runtime context")
            .contains("CONTEXT-PLUGIN"));

        config.normal_system_prompt_override = None;
        let parts = build_split_system_prompt(&config, &ctx);
        assert!(parts.base_system.contains("STYLE-PLUGIN"));
    }

    #[test]
    fn prompt_personas_retain_capability_and_safety_boundaries() {
        let minimal = build_minimal_prompt_parts(None);
        for required in [
            "Read the relevant REBON.md instructions before changing files",
            "Start with Bash for shell operations and Read for file access",
            "Consult ToolSearch only if you need a different tool",
            "complete schema",
            "Obtain explicit approval before any destructive action or action affecting shared state",
            "Keep secrets safe",
            "Check your changes and give an honest account of any failures",
        ] {
            assert!(minimal.base_system.contains(required), "{required}");
        }
        assert!(minimal.runtime_context.is_none());
        assert!(minimal.transient_context.is_none());
        let chat = build_chat_prompt_parts(None);
        for required in [
            "No tools are available to you in this session",
            "cannot access or change files, execute commands, or browse the web",
            "your words cannot alter the user's machine",
            "leave execution to them",
            "Never imply that you performed an action you have not taken",
            "Acknowledge uncertainty",
        ] {
            assert!(chat.base_system.contains(required), "{required}");
        }
        assert!(chat.runtime_context.is_none());
        assert!(chat.transient_context.is_none());
    }

    #[test]
    fn prompt_templates_substitute_values_without_changing_protocol_literals() {
        let plain = doing_tasks_section(None);
        let interactive = doing_tasks_section(Some("AskUserQuestion"));
        let escalation = "Ask the user for help only after investigation leaves you truly blocked";
        assert!(plain.contains(escalation));
        assert_eq!(
            interactive,
            plain.replace(escalation, "Ask the user for help through AskUserQuestion only after investigation leaves you truly blocked")
        );
        assert!(!interactive.contains("{name}"));
        assert!(!interactive.contains("{escalation}"));
        let scratch = scratchpad_section("C:/scratch/protocol-check");
        assert!(scratch.contains("`C:/scratch/protocol-check`"));
        assert!(!scratch.contains("{dir}"));
        let language = language_section("Chinese");
        assert_eq!(language.matches("Chinese").count(), 2);
        assert!(!language.contains("{language}"));
        assert!(
            language.contains("keep technical terms and code identifiers in their original form")
        );
    }

    #[test]
    fn using_tools_keeps_optional_tool_gates_and_task_priority() {
        let task_only = using_tools_section(&["TaskCreate".into()]);
        assert!(task_only.contains("Use TaskCreate to divide up and manage the work"));
        let both = using_tools_section(&["TodoWrite".into(), "TaskCreate".into()]);
        assert!(both.contains("Use TodoWrite to divide up and manage the work"));
        assert!(!both.contains("Use TaskCreate"));
        let empty = using_tools_section(&[]);
        assert!(!empty.contains("Use TodoWrite"));
        assert!(!empty.contains("Use TaskCreate"));
        let full = using_tools_section(&sample_config().tool_names);
        for required in [
            "Read files with Read, not cat, head, tail, or sed",
            "Change files with Edit, not sed or awk",
            "Create files with Write, not cat with heredoc or echo redirection",
            "Locate files with Glob, not find or ls",
            "Search within files with Grep, not grep or rg",
            "read-before-write, stale-content, or same-path serialization",
            "Edit, Write, MultiEdit, and NotebookEdit",
            "modified-since-read",
        ] {
            assert!(full.contains(required), "{required}");
        }
    }

    #[test]
    fn session_guidance_matches_for_eager_and_deferred_capabilities() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        let tools = [
            "Agent",
            "AskUserQuestion",
            "Glob",
            "Grep",
            "Monitor",
            "Skill",
        ]
        .map(str::to_string);
        for auto_continue in [true, false] {
            let eager = session_specific_guidance_section(&tools, &[], auto_continue).unwrap();
            let deferred = session_specific_guidance_section(&[], &tools, auto_continue).unwrap();
            assert_eq!(eager, deferred);
            for literal in [
                "subagent_type=Explore",
                "`name`",
                "`team_name`",
                "`cwd`",
                "`allowed_roots`",
                "`isolation`",
                "`allowed_tools`",
                "`run_in_background`",
                "SendMessage",
                "`! <command>`",
                "gcloud auth login",
                "`/<skill-name>`",
                "`/commit`",
                "`/loop`",
            ] {
                assert!(eager.contains(literal), "{literal}");
            }
            assert!(eager.contains("Sleep is reserved for an explicitly requested time delay or an external condition unable to emit a completion event"));
        }
    }

    #[test]
    fn minimal_prompt_override_keeps_minimal_context_empty() {
        let parts = build_minimal_prompt_parts(Some("custom minimal persona"));

        assert_eq!(parts.base_system, "custom minimal persona");
        assert!(parts.runtime_context.is_none());
        assert!(parts.transient_context.is_none());
        assert_eq!(
            build_minimal_system_prompt(Some("custom minimal persona")),
            "custom minimal persona"
        );
    }

    #[test]
    fn blank_prompt_overrides_use_the_built_in_defaults() {
        let mut config = sample_config();
        config.normal_system_prompt_override = Some(" \n\t ".to_string());
        assert!(build_base_system_prompt(&config).contains("# System"));
        assert_eq!(
            build_minimal_system_prompt(Some(" \n\t ")),
            MINIMAL_SYSTEM_PROMPT
        );
    }

    #[test]
    fn base_system_omits_tool_lists_while_runtime_context_includes_them() {
        let config = sample_config_with_deferred_tool();
        let ctx = sample_context();

        let base = build_base_system_prompt(&config);
        assert!(base.contains("# Using your tools"));
        assert!(base.contains("they are not mistakes or cleanup opportunities"));
        assert!(base.contains("otherwise discard changes simply because someone else made them"));
        assert!(base.contains("ToolSearch may expose additional capabilities"));
        assert!(base.contains("is unavailable because it is missing from the provider-visible list, consult ToolSearch"));
        assert!(base.contains("InvokeDeferredTool gateway"));
        assert!(!base.contains("FakeEagerToolForCacheTest"));
        assert!(!base.contains("FakeDeferredToolForCacheTest"));
        for runtime_tool in ["Read", "Write", "AskUserQuestion"] {
            assert!(!base.contains(runtime_tool));
        }
        assert!(!base.contains("Available tools:"));
        assert!(!base.contains("Deferred tools:"));
        assert!(!base.contains("The following deferred tools"));

        let runtime = build_runtime_context_block(&config, &ctx).expect("runtime context");
        assert!(runtime.contains("FakeEagerToolForCacheTest"));
        assert!(runtime.contains("FakeDeferredToolForCacheTest"));
        assert!(runtime.contains("If the requested capability corresponds to a tool below"));
        assert!(runtime.contains("calling ToolSearch with query \"select:<name>[,<name>...]\""));
        assert!(runtime.contains("invoke the tool by its own name"));
    }

    /// The engine's own headers. `# Tone and style` and `# Output
    /// efficiency` are `rebon-plugin-model-prompt`'s and arrive through
    /// the prompt seat; that crate's golden pins their presence and absence.
    #[test]
    fn build_system_prompt_contains_all_section_headers() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        let expected_headers = [
            "# System",
            "# Doing tasks",
            "# Executing actions with care",
            "# Using your tools",
            "# Environment",
        ];
        for header in expected_headers {
            assert!(prompt.contains(header), "Missing section header: {header}");
        }
    }

    #[test]
    fn env_info_includes_cwd_platform_shell_model() {
        let config = sample_config();
        let ctx = sample_context();
        let section = env_info_section(&config, &ctx);

        assert!(section.contains("/home/user/project"));
        assert!(section.contains("win32"));
        assert!(section.contains("bash"));
        assert!(section.contains("claude-opus-4-6"));
        assert!(section.contains("Claude Opus 4.6"));
    }

    #[test]
    fn git_status_section_rendered_when_populated() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            git_status: Some(
                "The git status below was captured at the beginning of this turn. \
                 It stays fixed throughout the turn. A newer gitStatus snapshot later \
                 in the conversation replaces this one as the current snapshot.\n\n\
                 Current branch: main\n\n\
                 Main branch (normally the branch to use for PRs): main\n\n\
                 Git user: Jane\n\n\
                 Status:\n M Cargo.toml\n\n\
                 Recent commits:\nabc1234 fix things"
                    .to_string(),
            ),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        assert!(prompt.contains("gitStatus: "));
        assert!(prompt.contains("Current branch: main"));
        assert!(prompt.contains("Main branch (normally the branch to use for PRs): main"));
        assert!(prompt.contains("Git user: Jane"));
        assert!(prompt.contains("Status:\n M Cargo.toml"));
        assert!(prompt.contains("Recent commits:\nabc1234 fix things"));
    }

    #[test]
    fn git_status_section_absent_when_none() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(!prompt.contains("gitStatus:"));
    }

    #[test]
    fn git_status_section_absent_when_empty() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            git_status: Some(String::new()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        assert!(!prompt.contains("gitStatus:"));
    }

    #[test]
    fn collect_git_status_against_live_repo_contains_expected_lines() {
        // Spin up a throwaway repo, commit one file, and confirm the
        // summary includes branch + status + recent-commits lines.
        // Skipped silently if `git` is not on PATH (CI images without
        // git shouldn't fail this test).
        if std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            return;
        }

        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-git-status-live-")
            .tempdir()
            .unwrap();
        let tmp = tmp_dir.path();
        let cwd = tmp.to_str().unwrap();

        // Boilerplate: init repo, set a committer identity (required
        // on fresh machines where `git config user.name` is unset),
        // write one file, commit it.
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(tmp)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        assert!(run(&["init", "-b", "main"]));
        assert!(run(&["config", "user.email", "t@example.com"]));
        assert!(run(&["config", "user.name", "Test User"]));
        std::fs::write(tmp.join("a.txt"), "hello").unwrap();
        assert!(run(&["add", "a.txt"]));
        assert!(run(&["commit", "-m", "init commit"]));
        // Add a second modification so `status --short` is non-empty.
        std::fs::write(tmp.join("b.txt"), "pending").unwrap();

        let got = collect_git_status(cwd).expect("summary must resolve for a live repo");
        assert!(got.contains("captured at the beginning of this turn"));
        assert!(got.contains("It stays fixed throughout the turn"));
        assert!(
            got.contains("A newer gitStatus snapshot later in the conversation replaces this one")
        );
        assert!(got.contains("Current branch: main"));
        assert!(got.contains("Git user: Test User"));
        assert!(got.contains("Status:\n"));
        // `b.txt` is untracked → `fallback b.txt` line.
        assert!(got.contains("?? b.txt"));
        assert!(got.contains("Recent commits:\n"));
        assert!(got.contains("init commit"));
    }

    #[test]
    fn collect_git_status_returns_none_outside_repo() {
        // A plain temp dir should not be a git repo — `check_is_git`
        // returns false so we short-circuit.
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-git-status-none-")
            .tempdir()
            .unwrap();
        let got = collect_git_status(tmp_dir.path().to_str().unwrap());
        assert!(got.is_none());
    }

    #[test]
    fn env_info_includes_git_status() {
        let config = sample_config();

        let ctx_git = DynamicPromptContext {
            is_git: true,
            ..sample_context()
        };
        let section = env_info_section(&config, &ctx_git);
        assert!(section.contains("Is a git repository: true"));

        let ctx_no_git = DynamicPromptContext {
            is_git: false,
            ..sample_context()
        };
        let section = env_info_section(&config, &ctx_no_git);
        assert!(section.contains("Is a git repository: false"));
    }

    #[test]
    fn env_info_includes_knowledge_cutoff() {
        let config = sample_config();
        let ctx = sample_context();
        let section = env_info_section(&config, &ctx);
        assert!(section.contains("May 2025"));
    }

    #[test]
    fn env_info_includes_session_date() {
        let config = sample_config();
        let ctx = sample_context();
        let section = env_info_section(&config, &ctx);
        assert!(section.contains("2026-04-10"));
    }

    #[test]
    fn env_info_windows_shell_hint() {
        for platform in ["win32", "windows"] {
            let config = SystemPromptConfig {
                platform: platform.to_string(),
                ..sample_config()
            };
            let section = env_info_section(&config, &sample_context());
            assert!(section.contains("write Unix rather than Windows shell syntax"));
            assert!(section.contains("/dev/null not NUL"));
        }

        let config_linux = SystemPromptConfig {
            platform: "linux".to_string(),
            ..sample_config()
        };
        let section = env_info_section(&config_linux, &sample_context());
        assert!(!section.contains("write Unix rather than Windows shell syntax"));
    }

    /// The hint follows the advertised shell tools, not the platform. A
    /// PowerShell-only Windows session told to write Unix syntax reaches for a
    /// Bash it does not have.
    #[test]
    fn env_info_windows_shell_hint_follows_the_advertised_tools() {
        let with_tools = |tools: &[&str]| SystemPromptConfig {
            platform: "win32".to_string(),
            tool_names: tools.iter().map(|t| (*t).to_string()).collect(),
            ..sample_config()
        };

        let powershell_only =
            env_info_section(&with_tools(&["PowerShell", "Read"]), &sample_context());
        assert!(powershell_only.contains("Write PowerShell syntax"));
        assert!(powershell_only.contains("Do not treat it as a POSIX shell"));
        assert!(!powershell_only.contains("paths with forward slashes"));

        let both = env_info_section(
            &with_tools(&["Bash", "PowerShell", "Read"]),
            &sample_context(),
        );
        assert!(both.contains("Bash and PowerShell are distinct tools"));
        assert!(both.contains("Use the syntax of the tool being invoked"));

        let bash_only = env_info_section(&with_tools(&["Bash", "Read"]), &sample_context());
        assert!(bash_only.contains("write Unix rather than Windows shell syntax"));
        assert!(bash_only.contains("/dev/null not NUL"));
        assert!(!bash_only.contains("PowerShell"));

        // No shell at all reads as the Bash case rather than a fourth wording:
        // nothing is advertised, so the hint is inert until one arrives.
        let neither = env_info_section(&with_tools(&["Read"]), &sample_context());
        assert!(neither.contains("write Unix rather than Windows shell syntax"));
    }

    #[test]
    fn using_tools_mentions_registered_tools() {
        let config = sample_config();
        let section = runtime_tools_section(&config.tool_names, &config.deferred_tool_names);
        assert!(section.contains("Read"));
        assert!(section.contains("Edit"));
        assert!(section.contains("Glob"));
        assert!(section.contains("Grep"));
        assert!(section.contains("TodoWrite"));
    }

    #[test]
    fn runtime_tools_section_omits_deferred_guidance_when_no_deferred_tools() {
        let tools = vec![
            "Agent".into(),
            "AskUserQuestion".into(),
            "ExitPlanMode".into(),
        ];
        let section = runtime_tools_section(&tools, &[]);
        assert!(section.contains("Provider-visible tools loaded in advance for this turn:"));
        assert!(!section.contains("not exhaustive"));
        assert!(!section.contains("discovered through ToolSearch"));
        assert!(!section.contains("The deferred tools below"));
    }

    #[test]
    fn runtime_tools_section_includes_deferred_gateway_guidance() {
        let tools = vec![
            "Read".into(),
            "ToolSearch".into(),
            "InvokeDeferredTool".into(),
        ];
        let deferred = vec!["SaveMemory".into()];
        let section = runtime_tools_section(&tools, &deferred);

        assert!(section.contains("not exhaustive"));
        assert!(section.contains("deferred tools below"));
        assert!(section.contains("ToolSearch with query \"select:<name>[,<name>...]\""));
        assert!(section.contains("SaveMemory"));
    }

    #[test]
    fn using_tools_with_empty_tool_list_still_valid() {
        let section = using_tools_section(&[]);
        assert!(section.contains("# Using your tools"));
        // Should still have the Bash guidance even with no tools
        assert!(section.contains("Bash"));
    }

    #[test]
    fn using_tools_without_glob_grep_omits_those_lines() {
        let tools = vec!["Bash".into(), "Read".into(), "Edit".into()];
        let section = using_tools_section(&tools);
        assert!(!section.contains("Locate files with Glob"));
        assert!(!section.contains("Search within files with Grep"));
    }

    #[test]
    fn using_tools_marks_same_path_file_calls_as_dependent() {
        let tools = vec![
            "Bash".into(),
            "Read".into(),
            "Edit".into(),
            "Write".into(),
            "MultiEdit".into(),
            "NotebookEdit".into(),
        ];
        let section = using_tools_section(&tools);

        assert!(section.contains("operating on one path are dependent"));
        assert!(section.contains("NotebookEdit"));
        assert!(section.contains("do not batch a read and mutation of the same path"));
        assert!(section.contains("modified-since-read"));
        assert!(section.contains("Shell commands and scripts must never circumvent"));
    }

    #[test]
    fn language_section_present_when_set() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            language: Some("Chinese".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        assert!(prompt.contains("# Language"));
        assert!(prompt.contains("Every reply must be in Chinese"));
    }

    #[test]
    fn language_section_absent_when_unset() {
        let config = sample_config();
        let ctx = sample_context(); // language: None
        let prompt = build_system_prompt(&config, &ctx);
        assert!(!prompt.contains("# Language"));
    }

    #[test]
    fn rebon_md_content_included_when_present() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            rebon_md_content: Some("# My project instructions\nDo X not Y.".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        assert!(prompt.contains("Do X not Y"));
    }

    #[test]
    fn rebon_md_content_excluded_when_empty() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            rebon_md_content: Some("".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        // Empty REBON.md should not add a blank section
        assert!(!prompt.contains("\n\n\n\n"));
    }

    #[test]
    fn mcp_instructions_included_when_present() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            mcp_instructions: Some("# MCP Server Instructions\nUse tool X for Y.".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        assert!(prompt.contains("MCP Server Instructions"));
    }

    #[test]
    fn sections_separated_by_double_newlines() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        // Each section should be separated by \n\n
        assert!(prompt.contains("\n\n# System"));
        assert!(prompt.contains("\n\n# Doing tasks"));
    }

    #[test]
    fn model_without_marketing_name_uses_model_id() {
        let config = SystemPromptConfig {
            model_marketing_name: None,
            ..sample_config()
        };
        let section = env_info_section(&config, &sample_context());
        assert!(section.contains("Your underlying model is claude-opus-4-6."));
    }

    #[test]
    fn model_without_knowledge_cutoff_omits_line() {
        let config = SystemPromptConfig {
            knowledge_cutoff: None,
            ..sample_config()
        };
        let section = env_info_section(&config, &sample_context());
        assert!(!section.contains("knowledge cutoff"));
    }

    #[test]
    fn minimal_config_produces_valid_prompt() {
        let config = SystemPromptConfig {
            model: "test-model".to_string(),
            model_marketing_name: None,
            knowledge_cutoff: None,
            tool_names: Vec::new(),
            deferred_tool_names: Vec::new(),
            platform: "linux".to_string(),
            shell: "bash".to_string(),
            os_version: "Linux 6.6".to_string(),
            language: None,
            normal_system_prompt_override: None,
            minimal_system_prompt_override: None,
            chat_system_prompt_override: None,
            auto_continue_background_agents: true,
        };
        let ctx = DynamicPromptContext::default();
        let prompt = build_system_prompt(&config, &ctx);
        assert!(!prompt.is_empty());
        assert!(prompt.contains("# System"));
        assert!(prompt.contains("# Environment"));
    }

    #[test]
    fn resolved_context_uses_local_session_date() {
        let temp = tempfile::tempdir().unwrap();
        let before = crate::attachments::current_local_iso_date();
        let context = resolve_dynamic_context_with_language(
            temp.path().to_str().unwrap(),
            None,
            None,
            false,
            false,
            None,
        );
        let after = crate::attachments::current_local_iso_date();

        assert!(
            context.session_date.as_ref() == Some(&before)
                || context.session_date.as_ref() == Some(&after)
        );
    }

    #[test]
    fn tool_result_clearing_reminder_always_present() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(prompt.contains("record in your response any important details"));
    }

    #[test]
    fn coordinator_mode_injects_prompt_when_enabled() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            coordinator_mode: true,
            worker_tools_context: Some("Workers have: Read, Bash".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        assert!(prompt.contains("coordinator"));
        assert!(prompt.contains("Workers have: Read, Bash"));
        assert!(prompt.contains("AskUserQuestion"));
        assert!(prompt.contains("Pre-Spawn Worktree Gate"));
        assert!(!prompt.contains("# Doing tasks"));
        assert!(!prompt.contains("## When NOT to use the Agent tool"));
    }

    #[test]
    fn coordinator_mode_mentions_worktree_when_config_enabled() {
        let config = sample_config();
        let ctx = DynamicPromptContext {
            coordinator_mode: true,
            coordinator_use_worktree: true,
            worker_tools_context: Some("Workers have: Read, Bash".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&config, &ctx);
        assert!(prompt.contains("isolated git worktree"));
        assert!(prompt.contains("worker-local branch"));
    }

    #[test]
    fn coordinator_mode_absent_when_disabled() {
        let config = sample_config();
        let ctx = sample_context(); // coordinator_mode: false
        let prompt = build_system_prompt(&config, &ctx);
        assert!(!prompt.contains("## 1. Coordinator Responsibilities"));
    }

    // ---------------------------------------------------------------
    // Agent / Team section coverage
    // ---------------------------------------------------------------

    fn config_with_tools(extra: &[&str]) -> SystemPromptConfig {
        let mut tools = sample_config().tool_names;
        for name in extra {
            tools.push(name.to_string());
        }
        SystemPromptConfig {
            tool_names: tools,
            ..sample_config()
        }
    }

    #[test]
    fn session_guidance_always_contains_bang_command_hint() {
        // The `!<command>` hint is unconditional — it applies to any
        // interactive-ish session, and the cost of showing it to a
        // non-interactive caller is negligible.
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(prompt.contains("# Session-specific guidance"));
        assert!(prompt.contains("`! <command>` at the prompt"));
        assert!(prompt.contains("gcloud auth login"));
    }

    #[test]
    fn session_guidance_includes_ask_user_bullet_when_registered() {
        let config = config_with_tools(&["AskUserQuestion"]);
        let prompt = build_system_prompt(&config, &sample_context());
        assert!(prompt.contains(
            "When the reason for a user's refusal of a tool call is unclear, ask them \
             through AskUserQuestion"
        ));
    }

    #[test]
    fn session_guidance_omits_ask_user_bullet_when_not_registered() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(!prompt.contains("through AskUserQuestion"));
    }

    #[test]
    fn session_guidance_includes_skill_bullet_when_registered() {
        let config = config_with_tools(&["Skill"]);
        let prompt = build_system_prompt(&config, &sample_context());
        assert!(prompt.contains("A user's `/<skill-name>` command"));
        assert!(prompt.contains("must be run through Skill"));
    }

    #[test]
    fn session_guidance_omits_skill_bullet_when_not_registered() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(!prompt.contains("A user's `/<skill-name>` command"));
    }

    struct SubAgentsEnabledGuard {
        prior: bool,
    }

    impl SubAgentsEnabledGuard {
        fn set(enabled: bool) -> Self {
            let prior = rebon_tool::sub_agents_enabled();
            rebon_tool::set_sub_agents_enabled(enabled);
            Self { prior }
        }
    }

    impl Drop for SubAgentsEnabledGuard {
        fn drop(&mut self) {
            rebon_tool::set_sub_agents_enabled(self.prior);
        }
    }

    #[test]
    fn session_guidance_includes_agent_bullets_when_agent_tool_registered() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        let config = config_with_tools(&["Agent"]);
        let prompt = build_system_prompt(&config, &sample_context());
        assert!(prompt.contains(
            "Choose a specialized agent through Agent when its description fits the task"
        ));
        assert!(prompt.contains("subagent_type=Explore"));
        assert!(prompt.contains("scope of codebase exploration is unknown"));
        assert!(prompt.contains("default for a one-shot Explore agent is foreground execution"));
        assert!(prompt.contains("Named Explore teammates instead operate asynchronously"));
        assert!(prompt.contains("Leave `run_in_background` unset for them too"));
        assert!(prompt.contains("`name` must never be paired with per-call `cwd`"));
        assert!(prompt.contains("Named teammates default to background execution"));
        assert!(prompt.contains("never set `run_in_background` to false just to wait"));
        assert!(prompt.contains("`allowed_roots`, `isolation`, or `allowed_tools`"));
        assert!(prompt.contains("`name` and `team_name` and use a one-shot sub-agent instead"));
        assert!(prompt.contains("Waiting on a background agent must never involve Sleep, polling, or repeated progress checks"));
        assert!(prompt.contains("completion automatically produces an event that opens a new turn"));
        assert!(prompt.contains("End your current turn immediately"));
        assert!(prompt.contains("do not prolong the session or keep the provider cache alive"));
        assert!(prompt.contains("no more than one inexpensive, targeted probe"));
        assert!(!prompt.contains("more than 3 queries"));
    }

    #[test]
    fn session_guidance_omits_agent_bullets_when_agent_tool_not_registered() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(!prompt.contains("subagent_type=Explore"));
        assert!(!prompt.contains("Choose a specialized agent through Agent"));
    }

    #[test]
    fn session_guidance_agent_bullet_suggests_glob_grep_when_both_available() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        // sample_config already has Glob + Grep.
        let config = config_with_tools(&["Agent"]);
        let section = session_specific_guidance_section(
            &config.tool_names,
            &config.deferred_tool_names,
            true,
        )
        .expect("section should emit when Agent is present");
        assert!(section.contains("Use Glob or Grep yourself"));
    }

    #[test]
    fn session_guidance_agent_bullet_falls_back_to_bash_when_search_tools_missing() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        // Neither Glob nor Grep is registered, so the guidance bullet
        // names the Bash search fallback.
        let tools: Vec<String> = vec!["Bash".into(), "Agent".into()];
        let section = session_specific_guidance_section(&tools, &[], true)
            .expect("section should emit when Agent is present");
        assert!(section.contains("`find` or `grep` using Bash"));
    }

    #[test]
    fn session_guidance_runtime_toggle_suppresses_agent_bullets_only() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        let config = config_with_tools(&["Agent"]);

        rebon_tool::set_sub_agents_enabled(true);
        let section_on = session_specific_guidance_section(
            &config.tool_names,
            &config.deferred_tool_names,
            true,
        )
        .expect("section with agent on");
        assert!(section_on.contains("subagent_type=Explore"));

        // Toggling sub-agents off must drop the Agent bullets but
        // keep the rest of the section (e.g. the `!<command>` hint).
        rebon_tool::set_sub_agents_enabled(false);
        let section_off = session_specific_guidance_section(
            &config.tool_names,
            &config.deferred_tool_names,
            true,
        )
        .expect("section should still exist for the !command bullet");
        assert!(!section_off.contains("subagent_type=Explore"));
        assert!(section_off.contains("`! <command>`"));
    }

    #[test]
    fn session_guidance_reserves_named_teammates_for_reusable_context() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        let config = config_with_tools(&["Agent"]);

        let section = session_specific_guidance_section(
            &config.tool_names,
            &config.deferred_tool_names,
            true,
        )
        .expect("section with agent on");

        assert!(
            section.contains("Set `name` only if you anticipate reusing its accumulated context")
        );
        assert!(section.contains("A one-off search, review, verification, or implementation must not receive a name merely as a label"));
        assert!(section.contains("leave `name` out for ordinary one-shot delegation"));
    }

    #[test]
    fn session_guidance_background_agent_bullet_tracks_auto_continue_capability() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        let config = config_with_tools(&["Agent"]);

        // A frontend with an idle loop (TUI) may promise the wake-up.
        let auto = session_specific_guidance_section(
            &config.tool_names,
            &config.deferred_tool_names,
            true,
        )
        .expect("section with agent on");
        assert!(auto.contains("completion automatically produces an event that opens a new turn"));

        // Frontends without one (ACP/headless) must not promise a turn
        // the runtime cannot start; the notification rides along with
        // the next user-initiated turn instead.
        let manual = session_specific_guidance_section(
            &config.tool_names,
            &config.deferred_tool_names,
            false,
        )
        .expect("section with agent on");
        assert!(!manual.contains("opens a new turn"));
        assert!(manual.contains("notifications appear at the beginning of your next turn"));
        assert!(manual.contains("must never involve Sleep, polling"));
    }

    #[test]
    fn teams_section_is_not_emitted() {
        // Team-coordination guidance lives in the TeamCreate tool
        // description, not in the system prompt.
        // Registering the tool must NOT produce a system-prompt
        // section — the tool description already carries the guidance,
        // and a section would cost per-turn tokens.
        let config = config_with_tools(&["TeamCreate"]);
        let prompt = build_system_prompt(&config, &sample_context());
        assert!(!prompt.contains("# Coordinating with teammates"));
        assert!(!prompt.contains("teammate"));
    }

    #[test]
    fn session_guidance_appears_before_tone_and_style() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        let config = config_with_tools(&["Agent"]);
        let prompt = build_system_prompt(&config, &sample_context());
        let guidance_pos = prompt
            .find("# Session-specific guidance")
            .expect("session-guidance section");
        let env_pos = prompt.find("# Environment").expect("environment section");
        assert!(env_pos < guidance_pos);
    }

    #[test]
    fn session_guidance_bullet_ordering_is_stable() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        // Expected order: AskUserQuestion → !<command> → Agent bullets → Skill.
        let config = config_with_tools(&["Agent", "AskUserQuestion", "Skill"]);
        let section = session_specific_guidance_section(
            &config.tool_names,
            &config.deferred_tool_names,
            true,
        )
        .expect("section");
        let ask_pos = section
            .find("through AskUserQuestion")
            .expect("AskUserQuestion bullet");
        let bang_pos = section.find("`! <command>`").expect("!<command> bullet");
        let agent_pos = section
            .find("Choose a specialized agent through Agent")
            .expect("Agent bullet");
        let skill_pos = section
            .find("A user's `/<skill-name>` command")
            .expect("Skill bullet");
        assert!(ask_pos < bang_pos);
        assert!(bang_pos < agent_pos);
        assert!(agent_pos < skill_pos);
    }

    // -----------------------------------------------------------------
    // Engineering-control bullet coverage
    // -----------------------------------------------------------------
    //
    // These tests pin the exact phrasings that downstream evals and
    // prompt-regression tests rely on. The string checks look brittle
    // at first glance, but the *point* of this layer is that the
    // wording is pinned verbatim — silent edits
    // here would be the bug, not the assertion.

    #[test]
    fn doing_tasks_has_verify_before_complete_bullet() {
        let section = doing_tasks_section(None);
        assert!(section.contains("Confirm that the work functions before declaring completion"));
        assert!(section.contains("run its test, execute its script, and inspect the output"));
        // The "can't verify → say so" half is load-bearing — without it
        // the bullet degenerates into cheerleading ("always verify!")
        // that models ignore when verification is genuinely impossible.
        assert!(section.contains("If verification is impossible"));
        assert!(
            section.contains("explicitly disclose that limitation instead of declaring success")
        );
    }

    #[test]
    fn doing_tasks_has_progress_tracker_reconciliation_bullet() {
        let section = doing_tasks_section(None);
        assert!(section.contains(
            "Before your final reply, bring any progress tracker for this work into agreement"
        ));
        assert!(section.contains("close completed items"));
        assert!(section.contains("keep unfinished items open with clear blockers"));
        assert!(!section.contains("taskId"));
        assert!(!section.contains("in_progress"));
    }

    #[test]
    fn doing_tasks_has_faithful_reporting_bullet() {
        let section = doing_tasks_section(None);
        assert!(section.contains("Give a truthful account of results"));
        assert!(section.contains("Never say \"all tests pass\" in the face of failure output"));
        // Counter-weight against over-hedging confirmed success:
        assert!(section.contains("Do not attach needless caveats to confirmed results"));
        assert!(section.contains("repeat verification already done"));
        assert!(section.contains("Aim for accuracy rather than defensive reporting"));
    }

    #[test]
    fn doing_tasks_has_comment_discipline_trio() {
        let section = doing_tasks_section(None);
        // WHY not WHAT.
        assert!(section.contains("By default, produce code without comments"));
        assert!(section.contains("Add a comment only to explain a non-obvious WHY"));
        assert!(
            section.contains("Do not narrate WHAT the code does"),
            "missing WHAT/WHY framing"
        );
        assert!(section
            .contains("Keep references to the current task, fix, or callers out of comments"));
        // Historical-context preservation.
        assert!(section.contains("Preserve existing comments unless"));
        assert!(section.contains("can preserve a constraint or past bug lesson"));
    }

    #[test]
    fn doing_tasks_dropped_weak_legacy_comment_line() {
        // The old "Only add comments where the logic isn't self-evident"
        // line has been superseded by the comment-discipline trio. If
        // it reappears we've either double-added it or lost the new
        // bullets.
        let section = doing_tasks_section(None);
        assert!(
            !section.contains("Only add comments where the logic isn't self-evident"),
            "weak legacy comment line must not be present"
        );
    }

    #[test]
    fn doing_tasks_has_collaborator_framing() {
        let section = doing_tasks_section(None);
        assert!(section.contains("misconception"));
        assert!(section
            .contains("Contribute as a collaborator rather than only executing instructions"));
    }

    #[test]
    fn doing_tasks_bullet_ordering_keeps_backcompat_hacks_last() {
        // The "avoid backwards-compatibility hacks" bullet must stay
        // after the verify/faithful-report pair so the section reads
        // as "finish the work honestly, then clean up" rather than
        // "clean up, then maybe verify".
        let section = doing_tasks_section(None);
        let verify_pos = section
            .find("Confirm that the work functions before declaring completion")
            .expect("verify bullet");
        let faithful_pos = section
            .find("Give a truthful account of results")
            .expect("faithful bullet");
        let backcompat_pos = section
            .find("backwards-compatibility hacks")
            .expect("backcompat bullet");
        assert!(
            verify_pos < faithful_pos,
            "verify must precede faithful-report"
        );
        assert!(
            faithful_pos < backcompat_pos,
            "faithful-report must precede backcompat cleanup"
        );
    }

    // -----------------------------------------------------------------
    // Scratchpad section
    // -----------------------------------------------------------------

    #[test]
    fn scratchpad_section_absent_when_dir_not_set() {
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(!prompt.contains("# Scratchpad Directory"));
    }

    #[test]
    fn scratchpad_section_absent_when_dir_empty() {
        let ctx = DynamicPromptContext {
            scratchpad_dir: Some(String::new()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&sample_config(), &ctx);
        assert!(!prompt.contains("# Scratchpad Directory"));
    }

    #[test]
    fn scratchpad_section_present_when_dir_set() {
        let ctx = DynamicPromptContext {
            scratchpad_dir: Some("/tmp/rebon/proj/sess/scratchpad".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&sample_config(), &ctx);
        assert!(prompt.contains("# Scratchpad Directory"));
        assert!(prompt.contains("/tmp/rebon/proj/sess/scratchpad"));
        // Core instruction:
        assert!(prompt.contains("never in `/tmp`"));
        assert!(prompt.contains("An explicit user request is the only exception allowing `/tmp`"));
    }

    #[test]
    fn scratchpad_section_isolated_from_project_tree() {
        let ctx = DynamicPromptContext {
            scratchpad_dir: Some("C:/scratch/123".to_string()),
            ..sample_context()
        };
        let section = scratchpad_section(ctx.scratchpad_dir.as_deref().unwrap());
        assert!(section.contains("belongs to the current session alone"));
        assert!(section.contains("separate from the user's project"));
        assert!(section.contains("permission prompts are not required"));
    }

    #[test]
    fn scratchpad_section_appears_after_coordinator_sections() {
        // When coordinator mode is on, both the coordinator prompt
        // and the scratchpad section are present. Scratchpad lives in
        // stable runtime context, so it follows the coordinator base
        // prompt but precedes volatile worker routing context.
        let ctx = DynamicPromptContext {
            coordinator_mode: true,
            worker_tools_context: Some("Workers have: Read".into()),
            scratchpad_dir: Some("/tmp/sp".to_string()),
            ..sample_context()
        };
        let prompt = build_system_prompt(&sample_config(), &ctx);
        let coord_pos = prompt
            .find("You are a **coordinator**")
            .expect("coordinator ctx");
        let scratch_pos = prompt.find("# Scratchpad Directory").expect("scratchpad");
        assert!(coord_pos < scratch_pos);
        let worker_pos = prompt.find("Workers have: Read").expect("worker ctx");
        assert!(scratch_pos < worker_pos);
    }

    #[test]
    fn every_session_with_an_id_is_told_where_its_scratchpad_is() {
        let g = HomeGuard::new();
        let cwd = isolated_cwd(&g, "resolve-scratchpad");
        let cwd = cwd.to_string_lossy().to_string();

        // An ordinary TUI / ACP / serve session, not a coordinator. A
        // session that is not told where to put temp files picks a place
        // itself, and the place it picks has been the user's config home.
        let ctx = resolve_dynamic_context_with_language(
            &cwd,
            Some("sess-scratch"),
            None,
            false,
            false,
            None,
        );
        let expected = scratchpad_dir_for(&cwd, "sess-scratch");
        assert_eq!(ctx.scratchpad_dir.as_deref(), Some(expected.as_str()));
        let prompt = build_system_prompt(&sample_config(), &ctx);
        assert!(prompt.contains("# Scratchpad Directory"));
        assert!(prompt.contains(&expected));

        // Without a session id there is no per-session directory to name,
        // and pointing every anonymous session at one shared path would be
        // worse than saying nothing.
        let anonymous = resolve_dynamic_context_with_language(&cwd, None, None, false, false, None);
        assert!(anonymous.scratchpad_dir.is_none());
        assert!(
            !build_system_prompt(&sample_config(), &anonymous).contains("# Scratchpad Directory")
        );
    }

    #[test]
    fn sub_agent_prompt_suffix_combines_notes_and_scratchpad() {
        let suffix = sub_agent_prompt_suffix("/tmp/rebon/proj/sess/scratchpad");
        assert!(suffix.starts_with("Notes:"));
        assert!(suffix.contains("# Scratchpad Directory"));
        assert!(suffix.contains("/tmp/rebon/proj/sess/scratchpad"));
    }

    // -----------------------------------------------------------------
    // Sub-agent notes block
    // -----------------------------------------------------------------

    #[test]
    fn sub_agent_notes_contains_all_four_notes() {
        let notes = sub_agent_notes_section();
        assert!(notes.starts_with("Notes:"));
        // Note 1: absolute paths because bash cwd resets.
        assert!(notes.contains("cwd is reset between bash calls"));
        assert!(notes.contains("absolute file paths"));
        // Note 2: share absolute file paths in final response.
        assert!(notes.contains(
            "Give task-relevant file paths in your final reply, always absolute and never relative"
        ));
        assert!(notes.contains("Quote code only if its exact text matters"));
        // Note 3: emoji ban for sub-agents.
        assert!(notes.contains("MUST keep communication with the user free of emojis"));
        // Note 4: no colon before tool calls.
        assert!(notes.contains("Do not end text preceding a tool call with a colon"));
        assert!(notes.contains("Let me read the file."));
    }

    #[test]
    fn sub_agent_notes_is_a_single_block_not_part_of_main_prompt() {
        // The sub-agent notes must NOT be part of the main system
        // prompt build — they're appended ad-hoc by the spawner.
        let prompt = build_system_prompt(&sample_config(), &sample_context());
        assert!(!prompt.contains("cwd is reset between bash calls"));
    }

    // -----------------------------------------------------------------
    // prime_injected_prompt_files
    // -----------------------------------------------------------------

    /// Cross-test mutex so HOME / USERPROFILE manipulations serialise
    /// under cargo's parallel test runner. Shares [`crate::test_env_lock`]
    /// with every other env-mutating test module in this crate.
    fn home_test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    /// Redirect `$HOME` / `$USERPROFILE` at a temp dir so the
    /// REBON.md and MEMORY.md lookups both hit writable test state.
    struct HomeGuard {
        _temp: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
        prev_disable_auto_memory: Option<std::ffi::OsString>,
        prev_simple: Option<std::ffi::OsString>,
        prev_coordinator_mode: Option<std::ffi::OsString>,
        prev_config_dir: Option<std::ffi::OsString>,
        home_path: std::path::PathBuf,
        config_home_path: Option<std::path::PathBuf>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new() -> Self {
            let _lock = home_test_lock();
            let temp = tempfile::tempdir().expect("create temp home");
            let home_path = temp.path().to_path_buf();
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            let prev_disable_auto_memory = std::env::var_os("REBON_DISABLE_AUTO_MEMORY");
            let prev_simple = std::env::var_os("REBON_SIMPLE");
            let prev_coordinator_mode = std::env::var_os("REBON_COORDINATOR_MODE");
            let prev_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            unsafe {
                std::env::set_var("HOME", &home_path);
                std::env::set_var("USERPROFILE", &home_path);
                std::env::remove_var("REBON_DISABLE_AUTO_MEMORY");
                std::env::remove_var("REBON_SIMPLE");
                std::env::remove_var("REBON_COORDINATOR_MODE");
                std::env::remove_var("REBON_CONFIG_DIR");
            }
            HomeGuard {
                _temp: temp,
                prev_home,
                prev_userprofile,
                prev_disable_auto_memory,
                prev_simple,
                prev_coordinator_mode,
                prev_config_dir,
                home_path,
                config_home_path: None,
                _lock,
            }
        }

        fn home(&self) -> &std::path::Path {
            &self.home_path
        }

        fn config_home(&self) -> std::path::PathBuf {
            self.config_home_path
                .clone()
                .unwrap_or_else(|| self.home().join(".rebon"))
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match self.prev_userprofile.take() {
                    Some(v) => std::env::set_var("USERPROFILE", v),
                    None => std::env::remove_var("USERPROFILE"),
                }
                match self.prev_disable_auto_memory.take() {
                    Some(v) => std::env::set_var("REBON_DISABLE_AUTO_MEMORY", v),
                    None => std::env::remove_var("REBON_DISABLE_AUTO_MEMORY"),
                }
                match self.prev_simple.take() {
                    Some(v) => std::env::set_var("REBON_SIMPLE", v),
                    None => std::env::remove_var("REBON_SIMPLE"),
                }
                match self.prev_coordinator_mode.take() {
                    Some(v) => std::env::set_var("REBON_COORDINATOR_MODE", v),
                    None => std::env::remove_var("REBON_COORDINATOR_MODE"),
                }
                match self.prev_config_dir.take() {
                    Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                    None => std::env::remove_var("REBON_CONFIG_DIR"),
                }
            }
        }
    }

    /// Allocate a cwd under the guard's home (so instruction discovery
    /// resolves under the redirected home) and make sure it's an isolated
    /// directory. Returns the cwd path.
    fn isolated_cwd(g: &HomeGuard, label: &str) -> std::path::PathBuf {
        let cwd = g.home().join("proj").join(label);
        std::fs::create_dir_all(&cwd).unwrap();
        cwd
    }

    fn seed_global_rebon_md(g: &HomeGuard, body: &str) -> std::path::PathBuf {
        let dir = g.config_home();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("REBON.md");
        std::fs::write(&path, body).unwrap();
        std::fs::canonicalize(path).unwrap()
    }

    fn seed_project_rebon_md(cwd: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = cwd.join("REBON.md");
        std::fs::write(&path, body).unwrap();
        std::fs::canonicalize(path).unwrap()
    }

    #[test]
    fn register_primes_nothing_when_no_memory_files_exist() {
        let g = HomeGuard::new();
        let cwd = isolated_cwd(&g, "empty");
        let cache = FileStateCache::new();
        prime_injected_prompt_files(&cache, &cwd.to_string_lossy(), None);
        // No REBON.md, no MEMORY.md on disk → cache stays empty (except
        // for any pre-existing entries, which this test starts without).
        assert!(
            cache.is_empty(),
            "cache should be empty, got len={}",
            cache.len()
        );
    }

    #[test]
    fn register_primes_global_rebon_md_verbatim() {
        let g = HomeGuard::new();
        let cwd = isolated_cwd(&g, "with-global");
        let body = "# Global rebon instructions\nBe terse.\n";
        let path = seed_global_rebon_md(&g, body);
        let cache = FileStateCache::new();

        prime_injected_prompt_files(&cache, &cwd.to_string_lossy(), None);

        let state = cache.get(&path).expect("cache entry for global REBON.md");
        assert_eq!(state.content, body, "verbatim bytes preserved");
        assert!(
            !state.is_partial_view,
            "unmodified REBON.md is not a partial view"
        );
        assert_eq!(state.offset, None);
        assert_eq!(state.limit, None);
    }

    #[test]
    fn register_primes_project_rebon_md_verbatim() {
        let g = HomeGuard::new();
        let cwd = isolated_cwd(&g, "with-project");
        let body = "# Project rebon instructions\nFollow the style guide.\n";
        let path = seed_project_rebon_md(&cwd, body);
        let cache = FileStateCache::new();

        prime_injected_prompt_files(&cache, &cwd.to_string_lossy(), None);

        let state = cache.get(&path).expect("cache entry for project REBON.md");
        assert_eq!(state.content, body);
        assert!(!state.is_partial_view);
    }

    #[test]
    fn register_sets_mtime_for_primed_entries() {
        let g = HomeGuard::new();
        let cwd = isolated_cwd(&g, "mtime-check");
        let path = seed_project_rebon_md(&cwd, "# Project\n");
        let cache = FileStateCache::new();

        prime_injected_prompt_files(&cache, &cwd.to_string_lossy(), None);

        let state = cache.get(&path).expect("cached");
        assert!(
            state.timestamp_ms > 0,
            "timestamp should be read from disk (got {})",
            state.timestamp_ms
        );
    }

    #[test]
    fn register_overwrites_stale_entry_with_fresh_disk_bytes() {
        // Covers the "persistent shared cache across turns" case: if a
        // prior turn cached a stale MEMORY.md, the next turn's priming
        // must refresh the entry so Edit/Write see current disk bytes.
        let g = HomeGuard::new();
        let cwd = isolated_cwd(&g, "overwrite");
        let path = seed_project_rebon_md(&cwd, "# v1\n");
        let cache = FileStateCache::new();

        // Simulate a stale entry left by a previous turn.
        cache.set(
            &path,
            FileState {
                content: "# stale\n".to_string(),
                timestamp_ms: 1,
                offset: Some(10),
                limit: Some(5),
                is_partial_view: true,
            },
        );

        // Bump to v2 on disk and re-prime.
        std::fs::write(&path, "# v2\n").unwrap();
        prime_injected_prompt_files(&cache, &cwd.to_string_lossy(), None);

        let state = cache.get(&path).expect("cached");
        assert_eq!(state.content, "# v2\n", "priming must refresh content");
        assert_eq!(state.offset, None, "priming must clear offset");
        assert_eq!(state.limit, None, "priming must clear limit");
        assert!(!state.is_partial_view, "REBON.md is not partial");
    }

    #[test]
    fn load_rebon_md_uses_discovery_order_and_stripping() {
        let g = HomeGuard::new();
        let parent = g.home().join("repo");
        let cwd = parent.join("child");
        std::fs::create_dir_all(&cwd).unwrap();
        seed_global_rebon_md(&g, "global\n");
        seed_project_rebon_md(&parent, "parent\n");
        seed_project_rebon_md(&cwd, "---\npaths: [src/**]\n---\n<!-- hidden -->\ncwd\n");

        let prompt = load_rebon_md(&cwd.to_string_lossy()).expect("prompt");
        let global = prompt.find("global").unwrap();
        let parent_pos = prompt.find("parent").unwrap();
        let cwd_pos = prompt.find("cwd").unwrap();
        assert!(global < parent_pos && parent_pos < cwd_pos);
        assert!(!prompt.contains("paths:"));
        assert!(!prompt.contains("hidden"));
    }

    #[test]
    fn register_flags_stripped_rebon_md_as_partial_and_keeps_raw() {
        let g = HomeGuard::new();
        let cwd = isolated_cwd(&g, "stripped-rebon");
        let raw = "---\npaths: [src/**]\n---\nvisible <!-- hidden -->\n";
        let path = seed_project_rebon_md(&cwd, raw);
        let cache = FileStateCache::new();

        prime_injected_prompt_files(&cache, &cwd.to_string_lossy(), None);

        let state = cache.get(&path).expect("cache entry for stripped REBON.md");
        assert_eq!(state.content, raw, "raw disk bytes preserved in cache");
        assert!(
            state.is_partial_view,
            "stripped injected view must be partial"
        );
    }
}
