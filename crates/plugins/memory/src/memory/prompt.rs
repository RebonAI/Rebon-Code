//! Auto-memory prompt system — scoped user/repo durable memory and MEMORY.md loading.
//!
//! Provides:
//!
//! - [`get_auto_mem_path`] — resolves the canonical repo-scope memory directory
//! - [`load_memory_prompt`] — orchestrates scoped dir creation + prompt assembly
//! - [`build_memory_prompt`] — builds behavioral instructions + MEMORY.md content
//! - [`truncate_entrypoint`] — caps MEMORY.md at 200 lines / 25 KB

use std::path::{Path, PathBuf};

use rebon_session::memory_paths::{self, MemoryScope, ENTRYPOINT_NAME};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max lines loaded from MEMORY.md before truncation.
const MAX_ENTRYPOINT_LINES: usize = 200;

/// Max bytes loaded from MEMORY.md before truncation.
const MAX_ENTRYPOINT_BYTES: usize = 25_000;

/// Guidance appended so the model knows the dir already exists.
const DIR_EXISTS_GUIDANCE: &str =
    "This directory already exists \u{2014} write to it directly with the Write tool \
     (do not run mkdir or check for its existence).";

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Return the rebon config-home directory.
pub fn get_rebon_home() -> Option<PathBuf> {
    rebon_session::config_home_with_env(|name| std::env::var_os(name))
}

/// Compute the canonical repo-scope auto-memory directory for a given cwd.
///
/// Layout: `<rebon-config-home>/projects/{sanitize_path(canonical-repo-or-cwd)}/memory/`.
///
/// Compatibility wrapper for existing callers.
pub fn get_auto_mem_path(cwd: &str) -> Option<PathBuf> {
    memory_paths::repo_memory_dir(cwd)
}

/// Create the memory directory if it doesn't exist. Idempotent.
///
/// Logs on failure but does not propagate — prompt building continues
/// even if the dir can't be created (the model's Write tool will
/// surface the real permission error).
pub fn ensure_memory_dir_exists(memory_dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(memory_dir) {
        eprintln!(
            "[memory] ensure_memory_dir_exists failed for {}: {e}",
            memory_dir.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Entrypoint truncation
// ---------------------------------------------------------------------------

/// Result of truncating MEMORY.md content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrypointTruncation {
    /// The (possibly truncated) content string.
    pub content: String,
    /// Original line count before truncation.
    pub line_count: usize,
    /// Original byte count before truncation.
    pub byte_count: usize,
    /// Whether the line cap fired.
    pub was_line_truncated: bool,
    /// Whether the byte cap fired.
    pub was_byte_truncated: bool,
}

/// Truncate MEMORY.md content to the line AND byte caps.
///
/// Line-truncates first (natural boundary), then byte-truncates at the
/// last newline before the cap. Appends a warning naming which cap fired.
pub fn truncate_entrypoint(raw: &str) -> EntrypointTruncation {
    let trimmed = raw.trim();
    let lines: Vec<&str> = trimmed.split('\n').collect();
    let line_count = lines.len();
    let byte_count = trimmed.len();

    let was_line_truncated = line_count > MAX_ENTRYPOINT_LINES;
    let was_byte_truncated = byte_count > MAX_ENTRYPOINT_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return EntrypointTruncation {
            content: trimmed.to_string(),
            line_count,
            byte_count,
            was_line_truncated,
            was_byte_truncated,
        };
    }

    let mut truncated = if was_line_truncated {
        lines[..MAX_ENTRYPOINT_LINES].join("\n")
    } else {
        trimmed.to_string()
    };

    if truncated.len() > MAX_ENTRYPOINT_BYTES {
        if let Some(cut_at) = truncated[..MAX_ENTRYPOINT_BYTES].rfind('\n') {
            truncated.truncate(cut_at);
        } else {
            truncated.truncate(MAX_ENTRYPOINT_BYTES);
        }
    }

    let reason = match (was_byte_truncated, was_line_truncated) {
        (true, false) => format!(
            "{} bytes (limit: {MAX_ENTRYPOINT_BYTES}) \u{2014} index entries are too long",
            byte_count
        ),
        (false, true) => format!("{line_count} lines (limit: {MAX_ENTRYPOINT_LINES})"),
        _ => format!("{line_count} lines and {byte_count} bytes"),
    };

    truncated.push_str(&format!(
        "\n\n> WARNING: {ENTRYPOINT_NAME} is {reason}. \
         Only part of it was loaded. Keep index entries to one line under \
         ~200 chars; move detail into topic files."
    ));

    EntrypointTruncation {
        content: truncated,
        line_count,
        byte_count,
        was_line_truncated,
        was_byte_truncated,
    }
}

// ---------------------------------------------------------------------------
// Memory prompt builder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryWriteMode {
    ManualFiles,
    SaveMemoryTool,
    RecallOnly,
}

/// Estimated-token cap for auto-loaded MEMORY.md index content.
///
/// The budget applies only to entrypoint/index bodies. Behavioral guidance,
/// scope headings, paths, and empty-entrypoint messages are always rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPromptBudget {
    /// Hard cap, in estimated tokens, for each MEMORY.md entrypoint body.
    /// Estimation follows the repository convention of roughly 4 bytes/token.
    pub max_entrypoint_index_tokens: usize,
}

impl MemoryPromptBudget {
    /// Conservative Phase E runtime default: about 1,200 estimated tokens per
    /// MEMORY.md index. This keeps broad startup injection bounded while still
    /// leaving room for compact headings and one-line index pointers.
    pub const fn default_runtime() -> Self {
        Self {
            max_entrypoint_index_tokens: 1_200,
        }
    }
}

/// Options controlling how much durable-memory guidance is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPromptOptions {
    /// Whether this prompt is for coordinator mode.
    pub is_coordinator: bool,
    /// Whether Write/Edit-style manual memory file updates are available and
    /// appropriate to describe in the prompt.
    pub can_write_memory: bool,
    /// Whether a dedicated SaveMemory-style tool is available.
    pub can_save_memory: bool,
    /// Optional prompt budget for MEMORY.md index content. `None` preserves the
    /// existing full-entrypoint behavior after the existing line/byte caps.
    pub budget: Option<MemoryPromptBudget>,
}

impl Default for MemoryPromptOptions {
    fn default() -> Self {
        Self {
            is_coordinator: false,
            can_write_memory: true,
            can_save_memory: false,
            budget: None,
        }
    }
}

impl MemoryPromptOptions {
    fn write_mode(self) -> MemoryWriteMode {
        if self.can_save_memory {
            MemoryWriteMode::SaveMemoryTool
        } else if self.can_write_memory && !self.is_coordinator {
            MemoryWriteMode::ManualFiles
        } else {
            MemoryWriteMode::RecallOnly
        }
    }
}

#[derive(Debug, Clone)]
struct ScopedEntrypoint {
    label: &'static str,
    dir: PathBuf,
    content: Option<String>,
}

/// Build the full memory prompt: behavioral instructions + MEMORY.md content.
///
/// `memory_dir_display` is the display path shown to the model (forward-slash
/// normalised on all platforms). Compatibility wrapper for a single repo scope.
pub fn build_memory_prompt(memory_dir_display: &str, entrypoint_content: Option<&str>) -> String {
    let scoped = ScopedEntrypoint {
        label: "repo-scope",
        dir: PathBuf::from(memory_dir_display),
        content: entrypoint_content.map(str::to_string),
    };
    build_scoped_memory_prompt(&[scoped], MemoryPromptOptions::default())
}

fn build_scoped_memory_prompt(
    entrypoints: &[ScopedEntrypoint],
    options: MemoryPromptOptions,
) -> String {
    let mut lines = build_memory_lines(entrypoints, options);

    for entrypoint in entrypoints {
        lines.push(String::new());
        lines.push(format!("## {ENTRYPOINT_NAME} — {}", entrypoint.label));
        lines.push(String::new());
        lines.push(format!("Path: `{}`", display_dir(&entrypoint.dir)));
        lines.push(String::new());
        match entrypoint.content.as_deref() {
            Some(raw) if !raw.trim().is_empty() => {
                let t = truncate_entrypoint(raw);
                lines.push(render_entrypoint_content(&t.content, options.budget));
            }
            _ => {
                lines.push(format!(
                    "This {ENTRYPOINT_NAME} is currently empty. When you save new memories in this scope, they will appear here."
                ));
            }
        }
    }

    lines.join("\n")
}

fn render_entrypoint_content(content: &str, budget: Option<MemoryPromptBudget>) -> String {
    let Some(budget) = budget else {
        return content.to_string();
    };
    budget_entrypoint_content(content, budget)
}

/// Return whether the already existing-truncated MEMORY.md entrypoint body would
/// be transformed by prompt budgeting.
///
/// Callers must pass the same post-[`truncate_entrypoint`] content used for
/// prompt rendering. This keeps cache/edit-safety decisions aligned with
/// [`render_entrypoint_content`] without duplicating its budget threshold logic.
pub fn entrypoint_content_is_budgeted_partial(
    truncated_content: &str,
    budget: MemoryPromptBudget,
) -> bool {
    estimated_tokens(truncated_content.trim()) > budget.max_entrypoint_index_tokens
}

fn budget_entrypoint_content(content: &str, budget: MemoryPromptBudget) -> String {
    let trimmed = content.trim();
    if !entrypoint_content_is_budgeted_partial(trimmed, budget) {
        return trimmed.to_string();
    }

    let hard_byte_cap = budget.max_entrypoint_index_tokens.saturating_mul(4);
    let mut kept = Vec::new();
    let mut omitted = 0usize;
    let mut used_bytes = 0usize;

    for line in trimmed.lines().map(str::trim_end) {
        let projected_bytes = if kept.is_empty() {
            line.len()
        } else {
            used_bytes.saturating_add(1).saturating_add(line.len())
        };
        if projected_bytes <= hard_byte_cap {
            kept.push(line.to_string());
            used_bytes = projected_bytes;
        } else if !line.trim().is_empty() {
            omitted += 1;
        }
    }

    let mut rendered = kept.join("\n").trim().to_string();
    if !rendered.is_empty() {
        rendered.push_str("\n\n");
    }
    rendered.push_str(&format!(
        "> NOTE: {omitted} MEMORY.md index entries omitted by prompt budget; use Read on MEMORY.md or linked files if needed."
    ));
    rendered
}

fn estimated_tokens(s: &str) -> usize {
    (s.len() + 3) / 4
}

/// Build the behavioral instruction lines (without MEMORY.md content).
fn build_memory_lines(
    entrypoints: &[ScopedEntrypoint],
    options: MemoryPromptOptions,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    let user_dir = entrypoints
        .iter()
        .find(|e| e.label == "user-scope")
        .map(|e| display_dir(&e.dir))
        .unwrap_or_else(|| "<unavailable>".to_string());
    let repo_dir = entrypoints
        .iter()
        .find(|e| e.label == "repo-scope")
        .map(|e| display_dir(&e.dir))
        .unwrap_or_else(|| "<unavailable>".to_string());

    // Header
    lines.push("# auto memory".to_string());
    lines.push(String::new());
    lines.push(format!(
        "You have a persistent, file-based memory system with durable scopes. User-scope memory is at `{user_dir}`; repo-scope memory is at `{repo_dir}`. {DIR_EXISTS_GUIDANCE}"
    ));
    lines.push(String::new());
    lines.push(
        "User-scope memory applies across repositories, coordinators, and sessions. Repo-scope memory applies to this canonical repo/project across coordinators, sessions, and worktrees."
            .to_string(),
    );
    lines.push(String::new());
    lines.push(
        "There is no durable task, session, or coordinator memory scope. Current progress belongs in tasks, reports, or scratchpad unless it is a durable high-level project memory worth future recall."
            .to_string(),
    );
    lines.push(String::new());
    lines.push(
        "Memory `type` (`user`, `feedback`, `project`, `reference`) is content taxonomy; storage scope (`user` or `repo`) is a separate recall boundary."
            .to_string(),
    );
    match options.write_mode() {
        MemoryWriteMode::ManualFiles => lines.push(
            "When manually saving memories, choose the user or repo path deliberately and keep one shared taxonomy; do not duplicate guidance per scope."
                .to_string(),
        ),
        MemoryWriteMode::SaveMemoryTool => lines.push(
            "When saving durable memory, prefer the SaveMemory tool and choose user or repo scope deliberately."
                .to_string(),
        ),
        MemoryWriteMode::RecallOnly => lines.push(
            "Use this section for recall context only. Do not manually Write/Edit memory files in coordinator mode; durable candidates can be promoted via the memory tool once available."
                .to_string(),
        ),
    }
    lines.push(String::new());
    if options.write_mode() == MemoryWriteMode::ManualFiles {
        lines.push(
            "You should build up this memory system over time so that future conversations \
         can have a complete picture of who the user is, how they'd like to collaborate \
         with you, what behaviors to avoid or repeat, and the context behind the work \
         the user gives you."
                .to_string(),
        );
        lines.push(String::new());
        lines.push(
            "If the user explicitly asks you to remember something, save it immediately as \
         whichever type fits best. If they ask you to forget something, find and remove \
         the relevant entry."
                .to_string(),
        );
        lines.push(String::new());
        lines.extend(types_section());
    } else {
        lines.push(
            "Use the loaded MEMORY.md indexes as compact recall context for future-relevant user preferences, feedback, project facts, and external references."
                .to_string(),
        );
        lines.push(String::new());
        lines.extend(compact_types_section());
    }

    if options.write_mode() == MemoryWriteMode::ManualFiles {
        // What NOT to save
        lines.extend(what_not_to_save_section());
        lines.push(String::new());

        // How to save memories
        lines.extend(how_to_save_section());
        lines.push(String::new());
    } else {
        lines.extend(compact_recall_rules_section(options.write_mode()));
        lines.push(String::new());
    }

    // When to access memories
    lines.extend(when_to_access_section());
    lines.push(String::new());

    // Before recommending from memory
    lines.extend(trusting_recall_section());
    lines.push(String::new());

    // Memory and other forms of persistence
    lines.push("## Memory and other forms of persistence".to_string());
    lines.push(
        "Memory is one of several persistence mechanisms available to you as you assist \
         the user in a given conversation. The distinction is often that memory can be \
         recalled in future conversations and should not be used for persisting information \
         that is only useful within the scope of the current conversation."
            .to_string(),
    );
    lines.push(
        "- When to use or update a plan instead of memory: If you are about to start a \
         non-trivial implementation task and would like to reach alignment with the user on \
         your approach you should use a Plan rather than saving this information to memory. \
         Similarly, if you already have a plan within the conversation and you have changed \
         your approach persist that change by updating the plan rather than saving a memory."
            .to_string(),
    );
    lines.push(
        "- When to use or update tasks instead of memory: When you need to break your work \
         in current conversation into discrete steps or keep track of your progress use tasks \
         instead of saving to memory. Tasks are great for persisting information about the \
         work that needs to be done in the current conversation, but memory should be reserved \
         for information that will be useful in future conversations."
            .to_string(),
    );
    lines.push(String::new());

    lines
}

/// `## Types of memory` — individual-only variant (no team/private scoping).
fn types_section() -> Vec<String> {
    vec![
        "## Types of memory".to_string(),
        String::new(),
        "There are several discrete types of memory that you can store in your memory system:".to_string(),
        String::new(),
        "<types>".to_string(),
        "<type>".to_string(),
        "    <name>user</name>".to_string(),
        "    <description>Contain information about the user's role, goals, responsibilities, and knowledge. Great user memories help you tailor your future behavior to the user's preferences and perspective. Your goal in reading and writing these memories is to build up an understanding of who the user is and how you can be most helpful to them specifically. For example, you should collaborate with a senior software engineer differently than a student who is coding for the very first time. Keep in mind, that the aim here is to be helpful to the user. Avoid writing memories about the user that could be viewed as a negative judgement or that are not relevant to the work you're trying to accomplish together.</description>".to_string(),
        "    <when_to_save>When you learn any details about the user's role, preferences, responsibilities, or knowledge</when_to_save>".to_string(),
        "    <how_to_use>When your work should be informed by the user's profile or perspective. For example, if the user is asking you to explain a part of the code, you should answer that question in a way that is tailored to the specific details that they will find most valuable or that helps them build their mental model in relation to domain knowledge they already have.</how_to_use>".to_string(),
        "    <examples>".to_string(),
        "    user: I'm a data scientist investigating what logging we have in place".to_string(),
        "    assistant: [saves user memory: user is a data scientist, currently focused on observability/logging]".to_string(),
        String::new(),
        "    user: I've been writing Go for ten years but this is my first time touching the React side of this repo".to_string(),
        "    assistant: [saves user memory: deep Go expertise, new to React and this project's frontend \u{2014} frame frontend explanations in terms of backend analogues]".to_string(),
        "    </examples>".to_string(),
        "</type>".to_string(),
        "<type>".to_string(),
        "    <name>feedback</name>".to_string(),
        "    <description>Guidance the user has given you about how to approach work \u{2014} both what to avoid and what to keep doing. These are a very important type of memory to read and write as they allow you to remain coherent and responsive to the way you should approach work in the project. Record from failure AND success: if you only save corrections, you will avoid past mistakes but drift away from approaches the user has already validated, and may grow overly cautious.</description>".to_string(),
        "    <when_to_save>Any time the user corrects your approach (\"no not that\", \"don't\", \"stop doing X\") OR confirms a non-obvious approach worked (\"yes exactly\", \"perfect, keep doing that\", accepting an unusual choice without pushback). Corrections are easy to notice; confirmations are quieter \u{2014} watch for them. In both cases, save what is applicable to future conversations, especially if surprising or not obvious from the code. Include *why* so you can judge edge cases later.</when_to_save>".to_string(),
        "    <how_to_use>Let these memories guide your behavior so that the user does not need to offer the same guidance twice.</how_to_use>".to_string(),
        "    <body_structure>Lead with the rule itself, then a **Why:** line (the reason the user gave \u{2014} often a past incident or strong preference) and a **How to apply:** line (when/where this guidance kicks in). Knowing *why* lets you judge edge cases instead of blindly following the rule.</body_structure>".to_string(),
        "    <examples>".to_string(),
        "    user: don't mock the database in these tests \u{2014} we got burned last quarter when mocked tests passed but the prod migration failed".to_string(),
        "    assistant: [saves feedback memory: integration tests must hit a real database, not mocks. Reason: prior incident where mock/prod divergence masked a broken migration]".to_string(),
        String::new(),
        "    user: stop summarizing what you just did at the end of every response, I can read the diff".to_string(),
        "    assistant: [saves feedback memory: this user wants terse responses with no trailing summaries]".to_string(),
        String::new(),
        "    user: yeah the single bundled PR was the right call here, splitting this one would've just been churn".to_string(),
        "    assistant: [saves feedback memory: for refactors in this area, user prefers one bundled PR over many small ones. Confirmed after I chose this approach \u{2014} a validated judgment call, not a correction]".to_string(),
        "    </examples>".to_string(),
        "</type>".to_string(),
        "<type>".to_string(),
        "    <name>project</name>".to_string(),
        "    <description>Information that you learn about ongoing work, goals, initiatives, bugs, or incidents within the project that is not otherwise derivable from the code or git history. Project memories help you understand the broader context and motivation behind the work the user is doing within this repository. Store project progress only for high-level ongoing work, decisions, or milestones that should survive new coordinators; do not store worker IDs, transient waiting state, current turn context, or temporary task details.</description>".to_string(),
        "    <when_to_save>When you learn who is doing what, why, or by when. These states change relatively quickly so try to keep your understanding of this up to date. Always convert relative dates in user messages to absolute dates when saving (e.g., \"Thursday\" \u{2192} \"2026-03-05\"), so the memory remains interpretable after time passes.</when_to_save>".to_string(),
        "    <how_to_use>Use these memories to more fully understand the details and nuance behind the user's request and make better informed suggestions.</how_to_use>".to_string(),
        "    <body_structure>Lead with the fact or decision, then a **Why:** line (the motivation \u{2014} often a constraint, deadline, or stakeholder ask) and a **How to apply:** line (how this should shape your suggestions). Project memories decay fast, so the why helps future-you judge whether the memory is still load-bearing.</body_structure>".to_string(),
        "    <examples>".to_string(),
        "    user: we're freezing all non-critical merges after Thursday \u{2014} mobile team is cutting a release branch".to_string(),
        "    assistant: [saves project memory: merge freeze begins 2026-03-05 for mobile release cut. Flag any non-critical PR work scheduled after that date]".to_string(),
        String::new(),
        "    user: the reason we're ripping out the old auth middleware is that legal flagged it for storing session tokens in a way that doesn't meet the new compliance requirements".to_string(),
        "    assistant: [saves project memory: auth middleware rewrite is driven by legal/compliance requirements around session token storage, not tech-debt cleanup \u{2014} scope decisions should favor compliance over ergonomics]".to_string(),
        "    </examples>".to_string(),
        "</type>".to_string(),
        "<type>".to_string(),
        "    <name>reference</name>".to_string(),
        "    <description>Stores pointers to where information can be found in external systems. These memories allow you to remember where to look to find up-to-date information outside of the project directory.</description>".to_string(),
        "    <when_to_save>When you learn about resources in external systems and their purpose. For example, that bugs are tracked in a specific project in Linear or that feedback can be found in a specific Slack channel.</when_to_save>".to_string(),
        "    <how_to_use>When the user references an external system or information that may be in an external system.</how_to_use>".to_string(),
        "    <examples>".to_string(),
        "    user: check the Linear project \"INGEST\" if you want context on these tickets, that's where we track all pipeline bugs".to_string(),
        "    assistant: [saves reference memory: pipeline bugs are tracked in Linear project \"INGEST\"]".to_string(),
        String::new(),
        "    user: the Grafana board at grafana.internal/d/api-latency is what oncall watches \u{2014} if you're touching request handling, that's the thing that'll page someone".to_string(),
        "    assistant: [saves reference memory: grafana.internal/d/api-latency is the oncall latency dashboard \u{2014} check it when editing request-path code]".to_string(),
        "    </examples>".to_string(),
        "</type>".to_string(),
        "</types>".to_string(),
        String::new(),
    ]
}

fn compact_types_section() -> Vec<String> {
    vec![
        "## Types of memory".to_string(),
        String::new(),
        "- `user`: user role, goals, responsibilities, knowledge, and collaboration preferences.".to_string(),
        "- `feedback`: user guidance about what to avoid or repeat.".to_string(),
        "- `project`: non-obvious high-level ongoing work, decisions, milestones, goals, bugs, or incidents for this repo; not transient task state.".to_string(),
        "- `reference`: pointers to external systems and where to find up-to-date information.".to_string(),
    ]
}

/// `## What NOT to save in memory`
fn what_not_to_save_section() -> Vec<String> {
    vec![
        "## What NOT to save in memory".to_string(),
        String::new(),
        "- Code patterns, conventions, architecture, file paths, or project structure \u{2014} these can be derived by reading the current project state.".to_string(),
        "- Git history, recent changes, or who-changed-what \u{2014} `git log` / `git blame` are authoritative.".to_string(),
        "- Debugging solutions or fix recipes \u{2014} the fix is in the code; the commit message has the context.".to_string(),
        "- Anything already documented in REBON.md files.".to_string(),
        "- Ephemeral task details: in-progress work, temporary state, current conversation context.".to_string(),
        String::new(),
        "These exclusions apply even when the user explicitly asks you to save. If they ask you to save a PR list or activity summary, ask what was *surprising* or *non-obvious* about it \u{2014} that is the part worth keeping. \"Remember that I fixed bug X today\" is repository history plus a repair recipe \u{2014} do not save it; say so, and offer to keep only a lasting lesson if the fix revealed one.".to_string(),
    ]
}

fn compact_recall_rules_section(write_mode: MemoryWriteMode) -> Vec<String> {
    let save_note = match write_mode {
        MemoryWriteMode::SaveMemoryTool => "- If something should become durable, use the memory tool concisely; do not hand-edit memory files unless explicitly instructed.",
        _ => "- If something looks like a durable memory candidate, note it for future memory-tool promotion; do not hand-edit memory files from coordinator mode.",
    };
    vec![
        "## Memory recall rules".to_string(),
        String::new(),
        "- Treat MEMORY.md entries as compact recall hints, not authoritative current state; verify stale claims before relying on them.".to_string(),
        "- Current task progress, transient state, worker IDs, and temporary details belong in tasks, reports, or scratchpad, not durable memory.".to_string(),
        save_note.to_string(),
        "- Never save what the repository already records \u{2014} code patterns, git history and recent fixes (\"I fixed bug X today\"), repair recipes, REBON.md content \u{2014} even on an explicit request; say why, and offer to keep only a lasting lesson if there is one.".to_string(),
    ]
}

/// `## How to save memories` — two-step process with MEMORY.md index.
fn how_to_save_section() -> Vec<String> {
    vec![
        "## How to save memories".to_string(),
        String::new(),
        "Saving a memory is a two-step process:".to_string(),
        String::new(),
        "**Step 1** \u{2014} write the memory to its own file (e.g., `user_role.md`, `feedback_testing.md`) using this frontmatter format:".to_string(),
        String::new(),
        "```markdown".to_string(),
        "---".to_string(),
        "name: {{memory name}}".to_string(),
        "description: {{one-line description \u{2014} used to decide relevance in future conversations, so be specific}}".to_string(),
        "type: {{user, feedback, project, reference}}".to_string(),
        "---".to_string(),
        String::new(),
        "{{memory content \u{2014} for feedback/project types, structure as: rule/fact, then **Why:** and **How to apply:** lines}}".to_string(),
        "```".to_string(),
        String::new(),
        format!("**Step 2** \u{2014} add a pointer to that file in `{ENTRYPOINT_NAME}`. `{ENTRYPOINT_NAME}` is an index, not a memory \u{2014} each entry should be one line, under ~150 characters: `- [Title](file.md) \u{2014} one-line hook`. It has no frontmatter. Never write memory content directly into `{ENTRYPOINT_NAME}`."),
        String::new(),
        format!("- `{ENTRYPOINT_NAME}` indexes are loaded into your conversation context subject to prompt budget \u{2014} lines after {MAX_ENTRYPOINT_LINES} and over-budget entries may be omitted, so keep the index concise and use Read on `{ENTRYPOINT_NAME}` or linked files when more detail is needed"),
        "- Keep the name, description, and type fields in memory files up-to-date with the content".to_string(),
        "- Organize memory semantically by topic, not chronologically".to_string(),
        "- Update or remove memories that turn out to be wrong or outdated".to_string(),
        "- Do not write duplicate memories. First check if there is an existing memory you can update before writing a new one.".to_string(),
    ]
}

/// `## When to access memories`
fn when_to_access_section() -> Vec<String> {
    vec![
        "## When to access memories".to_string(),
        "- When memories seem relevant, or the user references prior-conversation work.".to_string(),
        "- You MUST access memory when the user explicitly asks you to check, recall, or remember.".to_string(),
        "- If the user says to *ignore* or *not use* memory: proceed as if MEMORY.md were empty. Do not apply remembered facts, cite, compare against, or mention memory content.".to_string(),
        "- Memory records can become stale over time. Use memory as context for what was true at a given point in time. Before answering the user or building assumptions based solely on information in memory records, verify that the memory is still correct and up-to-date by reading the current state of the files or resources. If a recalled memory conflicts with current information, trust what you observe now \u{2014} and update or remove the stale memory rather than acting on it.".to_string(),
    ]
}

/// `## Before recommending from memory`
fn trusting_recall_section() -> Vec<String> {
    vec![
        "## Before recommending from memory".to_string(),
        String::new(),
        "A memory that names a specific function, file, or flag is a claim that it existed *when the memory was written*. It may have been renamed, removed, or never merged. Before recommending it:".to_string(),
        String::new(),
        "- If the memory names a file path: check the file exists.".to_string(),
        "- If the memory names a function or flag: grep for it.".to_string(),
        "- If the user is about to act on your recommendation (not just asking about history), verify first.".to_string(),
        String::new(),
        "\"The memory says X exists\" is not the same as \"X exists now.\"".to_string(),
        String::new(),
        "A memory that summarizes repo state (activity logs, architecture snapshots) is frozen in time. If the user asks about *recent* or *current* state, prefer `git log` or reading the code over recalling the snapshot.".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Top-level orchestrator
// ---------------------------------------------------------------------------

/// Load the memory prompt for a given cwd using default non-coordinator,
/// manual-write-capable options.
///
/// Orchestrates: resolve path → ensure dir → read MEMORY.md → build prompt.
/// Returns `None` if the home directory cannot be determined.
pub fn load_memory_prompt(cwd: &str) -> Option<String> {
    load_memory_prompt_with_options(cwd, MemoryPromptOptions::default())
}

/// Load the memory prompt for a given cwd with mode/capability-aware guidance.
pub fn load_memory_prompt_with_options(cwd: &str, options: MemoryPromptOptions) -> Option<String> {
    let user_dir = memory_paths::user_memory_dir()?;
    let repo_dir = memory_paths::repo_memory_dir(cwd)?;

    ensure_memory_dir_exists(&user_dir);
    ensure_memory_dir_exists(&repo_dir);
    // Repo memory used to be keyed by the sanitized cwd. This is the read
    // entry, so it is where the old directory is brought across — after which
    // there is only ever one repo directory to read.
    memory_paths::migrate_cwd_memory_dir(cwd);

    let entrypoints = vec![
        ScopedEntrypoint {
            label: "user-scope",
            dir: user_dir.clone(),
            content: std::fs::read_to_string(user_dir.join(ENTRYPOINT_NAME)).ok(),
        },
        ScopedEntrypoint {
            label: "repo-scope",
            dir: repo_dir.clone(),
            content: std::fs::read_to_string(repo_dir.join(ENTRYPOINT_NAME)).ok(),
        },
    ];

    Some(build_scoped_memory_prompt(&entrypoints, options))
}

// ---------------------------------------------------------------------------
// Cache-priming snapshot
// ---------------------------------------------------------------------------

/// Snapshot of the MEMORY.md entrypoint as auto-injected into the prompt.
///
/// Callers (notably `rebon-core`) use this
/// to prime `FileStateCache` with the RAW disk bytes while setting
/// `is_partial_view = content_differs_from_disk`, so Edit/Write can enforce
/// "must Read before Edit" when the model has only seen a transformed view.
///
/// * `path` — absolute path of the MEMORY.md file on disk.
/// * `raw_content` — unmodified bytes as read from disk.
/// * `content_differs_from_disk` — true iff the injected prompt content
/// would differ from `raw_content` (i.e. existing truncation or prompt-budget
/// omission fired) — the edit-safety comparison Edit/Write gate on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntrypointSnapshot {
    /// Absolute path of the MEMORY.md file on disk.
    pub path: std::path::PathBuf,
    /// Unmodified bytes as read from disk.
    pub raw_content: String,
    /// True iff the prompt-injected view would differ from `raw_content`
    /// (i.e. [`truncate_entrypoint`] would drop lines/bytes, or a supplied
    /// [`MemoryPromptBudget`] would omit non-empty entrypoint/index lines).
    pub content_differs_from_disk: bool,
}

/// Locate and load all MEMORY.md entrypoints for `cwd`, reporting whether
/// each injected prompt content would differ from disk.
///
/// Skips a directory that can't be resolved (no home) or whose file
/// doesn't exist. Does not create the directory — that's
/// [`load_memory_prompt`]'s responsibility.
///
/// With `budget: None`, `content_differs_from_disk` preserves the existing
/// behavior: it is true iff [`truncate_entrypoint`] would trim any lines/bytes.
/// With a budget, the flag also becomes true when prompt rendering would omit
/// entrypoint/index content after existing truncation. rebon does not yet strip
/// HTML comments or frontmatter from MEMORY.md, so only existing truncation and
/// explicit prompt budgeting drive the flag today — add other processors here
/// when they land.
pub fn load_memory_entrypoints_for_cache(cwd: &str) -> Vec<MemoryEntrypointSnapshot> {
    load_memory_entrypoints_for_cache_with_budget(cwd, None)
}

/// Locate and load all MEMORY.md entrypoints for `cwd`, accounting for the same
/// prompt budget used to render runtime MEMORY.md content.
pub fn load_memory_entrypoints_for_cache_with_budget(
    cwd: &str,
    budget: Option<MemoryPromptBudget>,
) -> Vec<MemoryEntrypointSnapshot> {
    let mut snapshots = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();

    memory_paths::migrate_cwd_memory_dir(cwd);
    for dir in [
        memory_paths::user_memory_dir(),
        memory_paths::repo_memory_dir(cwd),
    ]
    .into_iter()
    .flatten()
    {
        if dirs
            .iter()
            .any(|existing| same_display_path(existing, &dir))
        {
            continue;
        }
        dirs.push(dir);
    }

    for dir in dirs {
        let entrypoint_path = dir.join(ENTRYPOINT_NAME);
        if let Ok(raw_content) = std::fs::read_to_string(&entrypoint_path) {
            let t = truncate_entrypoint(&raw_content);
            let is_truncation_partial = t.was_line_truncated || t.was_byte_truncated;
            let is_budget_partial = budget
                .map(|budget| entrypoint_content_is_budgeted_partial(&t.content, budget))
                .unwrap_or(false);
            snapshots.push(MemoryEntrypointSnapshot {
                path: entrypoint_path,
                raw_content,
                content_differs_from_disk: is_truncation_partial || is_budget_partial,
            });
        }
    }

    snapshots
}

/// Compatibility wrapper returning the first available repo-scope MEMORY.md
/// snapshot for callers that still expect a single file.
pub fn load_memory_entrypoint_for_cache(cwd: &str) -> Option<MemoryEntrypointSnapshot> {
    load_memory_entrypoint_for_cache_with_budget(cwd, None)
}

/// Compatibility wrapper returning the first available repo-scope MEMORY.md
/// snapshot while accounting for prompt-budget omission.
pub fn load_memory_entrypoint_for_cache_with_budget(
    cwd: &str,
    budget: Option<MemoryPromptBudget>,
) -> Option<MemoryEntrypointSnapshot> {
    let entrypoint_path = memory_paths::memory_entrypoint_for_scope(MemoryScope::Repo, Some(cwd))?;
    let raw_content = std::fs::read_to_string(&entrypoint_path).ok()?;
    let t = truncate_entrypoint(&raw_content);
    let is_truncation_partial = t.was_line_truncated || t.was_byte_truncated;
    let is_budget_partial = budget
        .map(|budget| entrypoint_content_is_budgeted_partial(&t.content, budget))
        .unwrap_or(false);
    Some(MemoryEntrypointSnapshot {
        path: entrypoint_path,
        raw_content,
        content_differs_from_disk: is_truncation_partial || is_budget_partial,
    })
}

// ---------------------------------------------------------------------------
// Path checking
// ---------------------------------------------------------------------------

/// Check if an absolute path is within the auto-memory directory.
///
/// Normalises both paths before comparison to prevent traversal bypasses.
/// Returns `false` if the home directory cannot be determined.
pub fn is_auto_mem_path(absolute_path: &str, cwd: &str) -> bool {
    memory_paths::is_memory_path_for_any_scope(absolute_path, cwd)
}

fn display_dir(path: &Path) -> String {
    let display_path = path.to_string_lossy().replace('\\', "/");
    if display_path.ends_with('/') {
        display_path
    } else {
        format!("{display_path}/")
    }
}

fn same_display_path(a: &Path, b: &Path) -> bool {
    display_dir(a) == display_dir(b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_scoped_entries() -> Vec<ScopedEntrypoint> {
        vec![
            ScopedEntrypoint {
                label: "user-scope",
                dir: PathBuf::from("/home/user/.rebon/memory/user"),
                content: None,
            },
            ScopedEntrypoint {
                label: "repo-scope",
                dir: PathBuf::from("/home/user/.rebon/projects/slug/memory"),
                content: None,
            },
        ]
    }

    // -- Path resolution --

    #[test]
    fn get_auto_mem_path_contains_projects_and_memory() {
        if rebon_session::platform_home_dir().is_none() {
            return;
        }
        let path = get_auto_mem_path("/home/user/my-project").unwrap();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("projects"),
            "missing 'projects' segment: {path_str}"
        );
        assert!(
            path_str.ends_with("memory"),
            "should end with 'memory': {path_str}"
        );
    }

    #[test]
    fn get_auto_mem_path_uses_sanitized_slug() {
        if rebon_session::platform_home_dir().is_none() {
            return;
        }
        let path = get_auto_mem_path("/home/user/my-project").unwrap();
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("-home-user-my-project"));
    }

    #[test]
    fn get_auto_mem_path_honors_rebon_config_dir() {
        let _g = HomeGuard::with_config_dir();
        let cwd = "/home/user/my-project";
        let path = get_auto_mem_path(cwd).unwrap();
        let expected = _g
            .config_home()
            .join("projects")
            .join(rebon_session::session_storage::sanitize_path(cwd))
            .join("memory");

        assert_eq!(path, expected);
    }

    #[test]
    fn get_rebon_home_ends_with_dot_rebon() {
        let _g = HomeGuard::new();
        let home = get_rebon_home().unwrap();
        assert!(
            home.ends_with(".rebon"),
            "expected .rebon suffix: {}",
            home.display()
        );
    }

    // -- Truncation --

    #[test]
    fn truncate_within_limits_passes_through() {
        let content = "line 1\nline 2\nline 3";
        let t = truncate_entrypoint(content);
        assert_eq!(t.content, content);
        assert!(!t.was_line_truncated);
        assert!(!t.was_byte_truncated);
        assert_eq!(t.line_count, 3);
    }

    #[test]
    fn truncate_over_line_limit() {
        let lines: Vec<String> = (0..250).map(|i| format!("line {i}")).collect();
        let content = lines.join("\n");
        let t = truncate_entrypoint(&content);
        assert!(t.was_line_truncated);
        assert_eq!(t.line_count, 250);
        assert!(t.content.contains("WARNING"));
        assert!(t.content.contains("250 lines"));
        let content_before_warning = t.content.split("\n\n> WARNING").next().unwrap();
        let actual_lines = content_before_warning.split('\n').count();
        assert_eq!(actual_lines, MAX_ENTRYPOINT_LINES);
    }

    #[test]
    fn truncate_over_byte_limit() {
        let long_line = "x".repeat(30_000);
        let content = format!("line 1\n{long_line}");
        let t = truncate_entrypoint(&content);
        assert!(t.was_byte_truncated);
        assert!(t.content.contains("WARNING"));
        assert!(t.content.contains("index entries are too long"));
    }

    #[test]
    fn truncate_trims_whitespace() {
        let content = "  \n  line 1  \n  ";
        let t = truncate_entrypoint(content);
        assert_eq!(t.content, "line 1");
    }

    // -- Prompt builder --

    #[test]
    fn build_memory_lines_contains_all_section_headers() {
        let lines = build_memory_lines(&test_scoped_entries(), MemoryPromptOptions::default());
        let joined = lines.join("\n");
        let expected = [
            "# auto memory",
            "## Types of memory",
            "## What NOT to save in memory",
            "## How to save memories",
            "## When to access memories",
            "## Before recommending from memory",
            "## Memory and other forms of persistence",
        ];
        for header in expected {
            assert!(joined.contains(header), "Missing header: {header}");
        }
    }

    #[test]
    fn build_memory_lines_contains_memory_dir_path() {
        let lines = build_memory_lines(&test_scoped_entries(), MemoryPromptOptions::default());
        let joined = lines.join("\n");
        assert!(joined.contains("/home/user/.rebon/projects/slug/memory/"));
    }

    #[test]
    fn build_memory_lines_contains_dir_exists_guidance() {
        let lines = build_memory_lines(&test_scoped_entries(), MemoryPromptOptions::default());
        let joined = lines.join("\n");
        assert!(joined.contains("already exists"));
    }

    #[test]
    fn build_memory_lines_contains_all_four_memory_types() {
        let lines = build_memory_lines(&test_scoped_entries(), MemoryPromptOptions::default());
        let joined = lines.join("\n");
        assert!(joined.contains("<name>user</name>"));
        assert!(joined.contains("<name>feedback</name>"));
        assert!(joined.contains("<name>project</name>"));
        assert!(joined.contains("<name>reference</name>"));
    }

    #[test]
    fn build_memory_lines_contains_frontmatter_example() {
        let lines = build_memory_lines(&test_scoped_entries(), MemoryPromptOptions::default());
        let joined = lines.join("\n");
        assert!(joined.contains("```markdown"));
        assert!(joined.contains("name: {{memory name}}"));
        assert!(joined.contains("type: {{user, feedback, project, reference}}"));
    }

    #[test]
    fn build_memory_lines_references_entrypoint_name() {
        let lines = build_memory_lines(&test_scoped_entries(), MemoryPromptOptions::default());
        let joined = lines.join("\n");
        assert!(joined.contains(ENTRYPOINT_NAME));
    }

    #[test]
    fn build_memory_prompt_includes_scope_wording_and_no_durable_task_scope() {
        let prompt =
            build_scoped_memory_prompt(&test_scoped_entries(), MemoryPromptOptions::default());
        assert!(prompt.contains("User-scope memory applies across repositories"));
        assert!(prompt.contains("Repo-scope memory applies to this canonical repo/project"));
        assert!(prompt.contains("There is no durable task, session, or coordinator memory scope"));
        assert!(!prompt.contains("task-scope"));
        assert!(!prompt.contains("session-scope"));
        assert!(!prompt.contains("coordinator-scope"));
    }

    #[test]
    fn coordinator_prompt_uses_save_memory_guidance_without_manual_write_steps() {
        let prompt = build_scoped_memory_prompt(
            &test_scoped_entries(),
            MemoryPromptOptions {
                is_coordinator: true,
                can_write_memory: false,
                can_save_memory: true,
                budget: None,
            },
        );
        assert!(prompt.contains("prefer the SaveMemory tool"));
        assert!(prompt.contains("use the memory tool concisely"));
        assert!(!prompt.contains("Saving a memory is a two-step process"));
        assert!(!prompt.contains("name: {{memory name}}"));
        assert!(!prompt.contains("**Step 1**"));
    }

    // -- Full prompt --

    #[test]
    fn build_memory_prompt_with_empty_entrypoint() {
        let prompt = build_memory_prompt("/test/memory/", None);
        assert!(prompt.contains("# auto memory"));
        assert!(prompt.contains("currently empty"));
    }

    #[test]
    fn build_memory_prompt_with_content() {
        let content = "- [User role](user_role.md) \u{2014} senior backend engineer";
        let prompt = build_memory_prompt("/test/memory/", Some(content));
        assert!(prompt.contains("# auto memory"));
        assert!(prompt.contains("senior backend engineer"));
        assert!(!prompt.contains("currently empty"));
    }

    #[test]
    fn build_memory_prompt_with_whitespace_only_entrypoint() {
        let prompt = build_memory_prompt("/test/memory/", Some("   \n  \n  "));
        assert!(prompt.contains("currently empty"));
    }

    #[test]
    fn budget_keeps_scope_headers_and_paths_when_entrypoint_large() {
        let mut entries = test_scoped_entries();
        entries[1].content = Some(
            (0..40)
                .map(|i| format!("- [entry {i}](entry-{i}.md) \u{2014} {}", "x".repeat(80)))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let prompt = build_scoped_memory_prompt(
            &entries,
            MemoryPromptOptions {
                budget: Some(MemoryPromptBudget {
                    max_entrypoint_index_tokens: 40,
                }),
                ..MemoryPromptOptions::default()
            },
        );

        assert!(prompt.contains("# auto memory"));
        assert!(prompt.contains("## MEMORY.md \u{2014} repo-scope"));
        assert!(prompt.contains("Path: `/home/user/.rebon/projects/slug/memory/`"));
        assert!(prompt.contains("MEMORY.md index entries omitted by prompt budget"));
    }

    #[test]
    fn budget_omits_unrelated_index_lines_over_cap() {
        let mut entries = test_scoped_entries();
        entries[1].content = Some(
            (0..20)
                .map(|i| {
                    format!(
                        "- [entry {i}](entry-{i}.md) \u{2014} {}",
                        "detail".repeat(10)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let prompt = build_scoped_memory_prompt(
            &entries,
            MemoryPromptOptions {
                budget: Some(MemoryPromptBudget {
                    max_entrypoint_index_tokens: 35,
                }),
                ..MemoryPromptOptions::default()
            },
        );

        assert!(prompt.contains("- [entry 0](entry-0.md)"));
        assert!(!prompt.contains("- [entry 19](entry-19.md)"));
        assert!(prompt.contains("omitted by prompt budget"));
    }

    #[test]
    fn budget_reports_omitted_entry_count_deterministically() {
        let content = (0..6)
            .map(|i| {
                format!(
                    "- [entry {i}](entry-{i}.md) \u{2014} {}",
                    "abcdef".repeat(5)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let budget = MemoryPromptBudget {
            max_entrypoint_index_tokens: 20,
        };

        let first = budget_entrypoint_content(&content, budget);
        let second = budget_entrypoint_content(&content, budget);

        assert_eq!(first, second);
        assert!(first.contains("5 MEMORY.md index entries omitted by prompt budget"));
    }

    #[test]
    fn coordinator_prompt_budget_keeps_save_memory_guidance() {
        let mut entries = test_scoped_entries();
        entries[1].content = Some(
            (0..30)
                .map(|i| {
                    format!(
                        "- [entry {i}](entry-{i}.md) \u{2014} {}",
                        "detail".repeat(12)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let prompt = build_scoped_memory_prompt(
            &entries,
            MemoryPromptOptions {
                is_coordinator: true,
                can_write_memory: false,
                can_save_memory: true,
                budget: Some(MemoryPromptBudget {
                    max_entrypoint_index_tokens: 30,
                }),
            },
        );

        assert!(prompt.contains("prefer the SaveMemory tool"));
        assert!(prompt.contains("use the memory tool concisely"));
        assert!(!prompt.contains("Saving a memory is a two-step process"));
        assert!(prompt.contains("omitted by prompt budget"));
    }

    // -- load_memory_prompt integration --

    #[test]
    fn load_memory_prompt_returns_some_when_home_set() {
        if rebon_session::platform_home_dir().is_none() {
            return;
        }
        let result = load_memory_prompt("/tmp/test-project");
        assert!(result.is_some());
        let prompt = result.unwrap();
        assert!(prompt.contains("# auto memory"));
        assert!(prompt.contains("projects"));
    }

    // -- is_auto_mem_path --

    #[test]
    fn is_auto_mem_path_inside_memory_dir() {
        let _g = HomeGuard::new();
        let cwd = "/home/user/project";
        let mem_dir = get_auto_mem_path(cwd).unwrap();
        let test_path = mem_dir.join("user_role.md");
        assert!(is_auto_mem_path(&test_path.to_string_lossy(), cwd));
    }

    #[test]
    fn is_auto_mem_path_outside_memory_dir() {
        if rebon_session::platform_home_dir().is_none() {
            return;
        }
        assert!(!is_auto_mem_path("/etc/passwd", "/home/user/project"));
    }

    #[test]
    fn normalize_path_str_forward_slashes() {
        let result = display_dir(Path::new("C:\\Users\\test\\file"));
        assert!(result.contains("/"));
        assert!(!result.contains("\\"));
    }

    // -- load_memory_entrypoint_for_cache --

    /// Cross-test mutex so HOME / USERPROFILE / REBON_CONFIG_DIR manipulations serialise.
    /// cargo test runs tests in parallel by default and these tests redirect
    /// process-global env; a race would fail them non-deterministically.
    fn home_test_lock() -> &'static std::sync::Mutex<()> {
        crate::memory::test_env::env_test_lock()
    }

    /// Redirect `$HOME` / `$USERPROFILE` at a temp dir for the duration of
    /// the test so we can drop a synthetic MEMORY.md into the resolved
    /// `get_auto_mem_path` layout without disturbing the real user profile.
    struct HomeGuard {
        _temp: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
        prev_rebon_config_dir: Option<std::ffi::OsString>,
        home_path: PathBuf,
        config_home_path: Option<PathBuf>,
        // Held for the guard's lifetime so concurrent tests serialise.
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new() -> Self {
            // Panic-poisoned locks are fine: we only care about mutual
            // exclusion, not about the guarded state (``).
            let _lock = home_test_lock().lock().unwrap_or_else(|p| p.into_inner());
            let temp = tempfile::tempdir().expect("create temp home");
            let home_path = temp.path().to_path_buf();
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            let prev_rebon_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("HOME", &home_path);
            std::env::set_var("USERPROFILE", &home_path);
            std::env::remove_var("REBON_CONFIG_DIR");
            HomeGuard {
                _temp: temp,
                prev_home,
                prev_userprofile,
                prev_rebon_config_dir,
                home_path,
                config_home_path: None,
                _lock,
            }
        }

        fn with_config_dir() -> Self {
            let mut guard = Self::new();
            let config_home_path = guard.home_path.join("configured-rebon");
            std::env::set_var("REBON_CONFIG_DIR", &config_home_path);
            guard.config_home_path = Some(config_home_path);
            guard
        }

        fn home(&self) -> &Path {
            &self.home_path
        }

        fn config_home(&self) -> &Path {
            self.config_home_path.as_deref().unwrap_or(&self.home_path)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev_rebon_config_dir.take() {
                Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_userprofile.take() {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    fn seed_memory_md(cwd: &str, body: &str) -> PathBuf {
        let dir = get_auto_mem_path(cwd).expect("home resolved");
        std::fs::create_dir_all(&dir).expect("mkdir memdir");
        let path = dir.join(ENTRYPOINT_NAME);
        std::fs::write(&path, body).expect("write MEMORY.md");
        path
    }

    fn seed_dir_memory_md(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir memdir");
        let path = dir.join(ENTRYPOINT_NAME);
        std::fs::write(&path, body).expect("write MEMORY.md");
        path
    }

    #[test]
    fn load_memory_entrypoints_for_cache_returns_multiple_scopes() {
        let _g = HomeGuard::new();
        let cwd = "/virtual/project-multiple-memory";
        let user_path = seed_dir_memory_md(&memory_paths::user_memory_dir().unwrap(), "# user\n");
        let repo_path = seed_memory_md(cwd, "# repo\n");

        let snaps = load_memory_entrypoints_for_cache(cwd);
        let paths = snaps.iter().map(|s| s.path.clone()).collect::<Vec<_>>();
        assert_eq!(snaps.len(), 2);
        assert!(paths.contains(&user_path));
        assert!(paths.contains(&repo_path));
    }

    /// Where repo memory lived before the canonical key. Built here rather
    /// than exported, because reading it is exactly what no longer happens.
    /// The base comes off `user_memory_dir` (`<config home>/memory/user`) so
    /// the fixture cannot disagree with the resolver about the config home.
    fn cwd_keyed_memory_dir(cwd: &str) -> PathBuf {
        memory_paths::user_memory_dir()
            .unwrap()
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("projects")
            .join(rebon_session::sanitize_path(cwd))
            .join("memory")
    }

    /// The prompt has two scopes, and what the cwd-keyed directory held is in
    /// the repo one — moved there by the read entry, not read from two places.
    #[test]
    fn load_memory_prompt_brings_cwd_keyed_memory_into_the_repo_scope() {
        let _g = HomeGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let cwd_path = child.join("..").join("child");
        let cwd = cwd_path.to_string_lossy();

        let from = cwd_keyed_memory_dir(&cwd);
        assert_ne!(from, memory_paths::repo_memory_dir(&cwd).unwrap());
        seed_dir_memory_md(&from, "# carried over\n");

        let prompt = load_memory_prompt(&cwd).unwrap();
        assert!(!prompt.contains("cwd-scope"));
        assert!(prompt.contains("# carried over"));
        assert!(!from.exists());
        assert_eq!(
            std::fs::read_to_string(
                memory_paths::repo_memory_dir(&cwd)
                    .unwrap()
                    .join(ENTRYPOINT_NAME)
            )
            .unwrap(),
            "# carried over\n"
        );
    }

    #[test]
    fn load_memory_entrypoints_for_cache_has_one_snapshot_per_scope() {
        let _g = HomeGuard::new();
        let temp = tempfile::tempdir().expect("temp cwd");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let cwd_path = child.join("..").join("child");
        let cwd = cwd_path.to_string_lossy();
        seed_dir_memory_md(&memory_paths::user_memory_dir().unwrap(), "# user\n");
        seed_dir_memory_md(&memory_paths::repo_memory_dir(&cwd).unwrap(), "# repo\n");
        seed_dir_memory_md(&cwd_keyed_memory_dir(&cwd), "# from the old key\n");

        let snaps = load_memory_entrypoints_for_cache(&cwd);
        assert_eq!(snaps.len(), 2, "user and repo, never a third place");
        // The repo index was already there, so the incoming one lands beside
        // it rather than replacing it, and is not an entrypoint of its own.
        assert!(snaps.iter().any(|s| s.raw_content == "# repo\n"));
        assert_eq!(
            std::fs::read_to_string(
                memory_paths::repo_memory_dir(&cwd)
                    .unwrap()
                    .join("MEMORY.from-cwd-scope.md")
            )
            .unwrap(),
            "# from the old key\n"
        );
    }

    #[test]
    fn load_memory_entrypoint_for_cache_returns_none_when_missing() {
        let _g = HomeGuard::new();
        let cwd = "/virtual/project-missing-memory";
        // Ensure the parent dir exists but the file does not.
        let dir = get_auto_mem_path(cwd).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_memory_entrypoint_for_cache(cwd).is_none());
    }

    #[test]
    fn load_memory_entrypoint_for_cache_no_truncation_clears_flag() {
        let g = HomeGuard::new();
        let cwd = "/virtual/project-small-memory";
        let body = "# small\n- [a](a.md)\n- [b](b.md)\n";
        let path = seed_memory_md(cwd, body);
        let snap = load_memory_entrypoint_for_cache(cwd).expect("snapshot");
        assert_eq!(snap.path, path);
        assert_eq!(snap.raw_content, body);
        assert!(!snap.content_differs_from_disk, "no truncation expected");
        // Sanity: the home guard actually rewrote the path.
        assert!(
            snap.path.starts_with(g.home()),
            "snapshot path must be under the redirected HOME: {}",
            snap.path.display()
        );
    }

    #[test]
    fn load_memory_entrypoint_for_cache_line_truncation_sets_flag() {
        let _g = HomeGuard::new();
        let cwd = "/virtual/project-line-truncated";
        // One line over the cap.
        let body = (0..=MAX_ENTRYPOINT_LINES)
            .map(|i| format!("- [entry {i}](e{i}.md)"))
            .collect::<Vec<_>>()
            .join("\n");
        seed_memory_md(cwd, &body);
        let snap = load_memory_entrypoint_for_cache(cwd).expect("snapshot");
        assert!(
            snap.content_differs_from_disk,
            "line truncation must set content_differs_from_disk"
        );
        assert_eq!(snap.raw_content, body, "raw bytes must be preserved");
    }

    #[test]
    fn load_memory_entrypoint_for_cache_with_budget_marks_over_budget_partial() {
        let _g = HomeGuard::new();
        let cwd = "/virtual/project-budget-partial";
        let body = (0..80)
            .map(|i| format!("- [entry {i}](e{i}.md) — {}", "detail".repeat(10)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.split('\n').count() <= MAX_ENTRYPOINT_LINES);
        assert!(body.len() <= MAX_ENTRYPOINT_BYTES);
        seed_memory_md(cwd, &body);

        let snap = load_memory_entrypoint_for_cache_with_budget(
            cwd,
            Some(MemoryPromptBudget::default_runtime()),
        )
        .expect("snapshot");

        assert!(
            snap.content_differs_from_disk,
            "runtime prompt-budget omission must mark cache snapshot partial"
        );
        assert_eq!(snap.raw_content, body, "raw bytes must remain cached");
    }

    #[test]
    fn load_memory_entrypoint_for_cache_with_budget_keeps_under_budget_non_partial() {
        let _g = HomeGuard::new();
        let cwd = "/virtual/project-budget-small";
        let body = "- [a](a.md) — compact hook\n- [b](b.md) — compact hook\n";
        seed_memory_md(cwd, body);

        let snap = load_memory_entrypoint_for_cache_with_budget(
            cwd,
            Some(MemoryPromptBudget::default_runtime()),
        )
        .expect("snapshot");

        assert!(
            !snap.content_differs_from_disk,
            "under-budget untruncated entrypoint must not be partial"
        );
        assert_eq!(snap.raw_content, body);
    }

    #[test]
    fn load_memory_entrypoint_for_cache_none_budget_preserves_truncation_non_partial() {
        let _g = HomeGuard::new();
        let cwd = "/virtual/project-budget-none-truncation";
        let body = (0..80)
            .map(|i| format!("- [entry {i}](e{i}.md) — {}", "detail".repeat(10)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.split('\n').count() <= MAX_ENTRYPOINT_LINES);
        assert!(body.len() <= MAX_ENTRYPOINT_BYTES);
        seed_memory_md(cwd, &body);

        let snap = load_memory_entrypoint_for_cache_with_budget(cwd, None).expect("snapshot");

        assert!(
            !snap.content_differs_from_disk,
            "None budget must preserve the line/byte-only partial semantics"
        );
        assert_eq!(snap.raw_content, body);
    }

    #[test]
    fn load_memory_entrypoint_for_cache_byte_truncation_sets_flag() {
        let _g = HomeGuard::new();
        let cwd = "/virtual/project-byte-truncated";
        // A few long lines whose total bytes blow past the byte cap
        // while staying under the line cap.
        let line = format!("- [{}]({}.md)", "x".repeat(1000), "y".repeat(200));
        let body = std::iter::repeat(line.as_str())
            .take(30)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.len() > MAX_ENTRYPOINT_BYTES,
            "test body should exceed byte cap"
        );
        assert!(
            body.split('\n').count() <= MAX_ENTRYPOINT_LINES,
            "test body should stay under line cap"
        );
        seed_memory_md(cwd, &body);
        let snap = load_memory_entrypoint_for_cache(cwd).expect("snapshot");
        assert!(snap.content_differs_from_disk);
        assert_eq!(snap.raw_content, body);
    }
}
