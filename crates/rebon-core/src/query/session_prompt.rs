use super::*;

use crate::prompt_seat::{PluginPromptSection, Rung};
use crate::system_prompt::PromptPlane;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct SessionPromptCacheKey {
    model: String,
    model_marketing_name: Option<String>,
    knowledge_cutoff: Option<String>,
    platform: String,
    shell: String,
    os_version: String,
    normal_system_prompt_override: Option<String>,
    coordinator_mode: bool,
    coordinator_use_worktree: bool,
    coordinator_simple_mode: bool,
    sub_agents_enabled: bool,
    /// The plugin sections on base rungs, as the turn saw them. They are
    /// identity like everything else in this key: a plugin registering,
    /// unregistering, or answering differently for this turn's subject is
    /// a different base prompt, and the frozen one must not be served for
    /// it. The key holds the sections rather than a table generation so a
    /// change that renders the same bytes stays a hit.
    plugin_base_sections: Vec<PluginPromptSection>,
}

impl SessionPromptCacheKey {
    pub(super) fn from_config(config: &SystemPromptConfig, ctx: &DynamicPromptContext) -> Self {
        Self {
            model: config.model.clone(),
            model_marketing_name: config.model_marketing_name.clone(),
            knowledge_cutoff: config.knowledge_cutoff.clone(),
            platform: config.platform.clone(),
            shell: config.shell.clone(),
            os_version: config.os_version.clone(),
            normal_system_prompt_override: config.normal_system_prompt_override.clone(),
            coordinator_mode: ctx.coordinator_mode,
            coordinator_use_worktree: ctx.coordinator_use_worktree,
            coordinator_simple_mode: coordinator_simple_mode_enabled(),
            sub_agents_enabled: rebon_tool::sub_agents_enabled(),
            plugin_base_sections: plugin_sections_on(ctx, PromptPlane::Base),
        }
    }
}

/// Cache key for the frozen stable runtime-context block.
///
/// Extends the base-system key with every *identity* input of the block
/// (cwd, git-ness, language, session date, scratchpad) plus the
/// announced tool shape — a tool-filter change already invalidates the
/// provider's prefix cache through the request `tools` array, so
/// regenerating the block then is free.
///
/// Deliberately excluded are the block's *content* inputs
/// (`rebon_md_content`, the [`Rung::Memory`] sections, `mcp_instructions`):
/// for an unchanged identity the block must stay byte-identical for the whole
/// session even if those disk-backed inputs change underneath, because
/// it rides at message index 0 and any edit to it re-prefills the
/// entire conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct RuntimeContextCacheKey {
    base: SessionPromptCacheKey,
    cwd: String,
    is_git: bool,
    language: Option<String>,
    session_date: Option<String>,
    scratchpad_dir: Option<String>,
    tool_names: Vec<String>,
    deferred_tool_names: Vec<String>,
    /// The plugin sections on this plane's rung are IDENTITY, like the tool
    /// shape: a plugin registering or unregistering a section is a change
    /// to what the session is, not disk-state drift underneath it, so
    /// re-rendering the frozen block is the correct behavior. (Base-rung
    /// sections key the base plane the same way, in
    /// [`SessionPromptCacheKey`].)
    ///
    /// [`Rung::Memory`] is the one exception, and for the same reason
    /// `rebon_md_content` is excluded: what stands there is the *content* of
    /// documents on disk, which change under a session that is otherwise
    /// unchanged. Keying on it would rewrite message index 0 every time the
    /// user edited a memory file. Those edits reach the model through the
    /// nested-memory announcement instead ([`AnnouncedDocSnapshot`]).
    plugin_stable_sections: Vec<PluginPromptSection>,
}

impl RuntimeContextCacheKey {
    fn from_config(
        base: SessionPromptCacheKey,
        config: &SystemPromptConfig,
        ctx: &DynamicPromptContext,
    ) -> Self {
        Self {
            base,
            cwd: ctx.cwd.clone(),
            is_git: ctx.is_git,
            language: ctx.language.clone(),
            session_date: ctx.session_date.clone(),
            scratchpad_dir: ctx.scratchpad_dir.clone(),
            tool_names: config.tool_names.clone(),
            deferred_tool_names: config.deferred_tool_names.clone(),
            plugin_stable_sections: plugin_sections_on(ctx, PromptPlane::Stable)
                .into_iter()
                .filter(|section| section.rung != Rung::Memory)
                .collect(),
        }
    }
}

/// The turn's plugin sections whose rung renders on `plane`.
fn plugin_sections_on(ctx: &DynamicPromptContext, plane: PromptPlane) -> Vec<PluginPromptSection> {
    ctx.plugin_prompt_sections
        .iter()
        .filter(|section| section.rung.plane() == plane)
        .cloned()
        .collect()
}

/// The content inputs of the frozen runtime-context block as the
/// session was last told them — either through the frozen block itself
/// (session start) or through a later nested-memory update reminder.
///
/// The freeze in [`SessionPromptState::get_or_insert_runtime_context`]
/// deliberately ignores mid-session disk edits to these inputs so that
/// message index 0 stays byte-stable. This snapshot is the other half
/// of that contract: diffing against it lets the executor announce an
/// edit exactly once, append-only, through the nested-memory
/// attachment channel — the model still learns about the change, and
/// the prompt prefix never gets rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AnnouncedDocSnapshot {
    pub(super) rebon_md_content: Option<String>,
    pub(super) memory_prompt: Option<String>,
}

impl AnnouncedDocSnapshot {
    pub(super) fn from_ctx(ctx: &DynamicPromptContext) -> Self {
        Self {
            rebon_md_content: ctx.rebon_md_content.clone(),
            memory_prompt: memory_rung_text(ctx),
        }
    }
}

/// The turn's text on [`Rung::Memory`], joined the way the plane joins it.
///
/// The engine reads the *rung*, not a plugin: the announce contract is about
/// a place in the frozen block — "loaded documents, between the instruction
/// files and the MCP instructions" — and whatever stands there is what a
/// mid-session change has to be announced for. `None` when nothing does,
/// which is also what "auto-memory is off" looks like.
fn memory_rung_text(ctx: &DynamicPromptContext) -> Option<String> {
    let mut sections: Vec<PluginPromptSection> = ctx
        .plugin_prompt_sections
        .iter()
        .filter(|section| section.rung == Rung::Memory && !section.text.is_empty())
        .cloned()
        .collect();
    if sections.is_empty() {
        return None;
    }
    crate::prompt_seat::sort_sections(&mut sections);
    Some(
        sections
            .into_iter()
            .map(|section| section.text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

/// Attachment header for a mid-session REBON.md refresh (rendered by
/// the nested-memory producer as `Contents of {this}:`).
pub(super) const REBON_MD_UPDATE_DISPLAY_PATH: &str =
    "REBON.md (updated mid-session — supersedes the earlier copy)";
/// Attachment header for a mid-session auto-memory refresh.
pub(super) const MEMORY_UPDATE_DISPLAY_PATH: &str =
    "auto-memory (updated mid-session — supersedes the earlier copy)";

#[derive(Default)]
pub(super) struct SessionPromptState {
    pub(super) base_system_by_key: Mutex<HashMap<SessionPromptCacheKey, Arc<str>>>,
    runtime_context_by_key: Mutex<HashMap<RuntimeContextCacheKey, Option<Arc<str>>>>,
    announced_docs: Mutex<Option<AnnouncedDocSnapshot>>,
}

impl SessionPromptState {
    pub(super) fn get_or_insert_base_system<F>(
        &self,
        key: SessionPromptCacheKey,
        builder: F,
    ) -> (Arc<str>, bool)
    where
        F: FnOnce() -> String,
    {
        let mut guard = self
            .base_system_by_key
            .lock()
            .expect("session prompt cache lock poisoned");
        if let Some(cached) = guard.get(&key) {
            return (cached.clone(), true);
        }
        let value: Arc<str> = Arc::from(builder());
        guard.insert(key, value.clone());
        (value, false)
    }

    /// Diff the current content inputs against what the session was
    /// last told and update the record. Returns one nested-memory
    /// trigger per changed input. The first call of a session only
    /// records the baseline — the frozen runtime context already
    /// carries that content — and a removed input (now `None`) updates
    /// the baseline without a trigger.
    pub(super) fn doc_update_triggers(
        &self,
        current: AnnouncedDocSnapshot,
    ) -> Vec<crate::attachment_seat::NestedMemoryTrigger> {
        let mut guard = self
            .announced_docs
            .lock()
            .expect("announced docs lock poisoned");
        let Some(previous) = guard.as_ref() else {
            *guard = Some(current);
            return Vec::new();
        };
        let mut triggers = Vec::new();
        if previous.rebon_md_content != current.rebon_md_content {
            if let Some(content) = current.rebon_md_content.clone() {
                triggers.push(crate::attachment_seat::NestedMemoryTrigger {
                    display_path: REBON_MD_UPDATE_DISPLAY_PATH.to_string(),
                    content,
                });
            }
        }
        if previous.memory_prompt != current.memory_prompt {
            if let Some(content) = current.memory_prompt.clone() {
                triggers.push(crate::attachment_seat::NestedMemoryTrigger {
                    display_path: MEMORY_UPDATE_DISPLAY_PATH.to_string(),
                    content,
                });
            }
        }
        *guard = Some(current);
        triggers
    }

    /// Freeze the stable runtime-context block per (base key, tool
    /// shape). See [`RuntimeContextCacheKey`] for why later disk-state
    /// drift must not re-render this block mid-session.
    pub(super) fn get_or_insert_runtime_context<F>(
        &self,
        key: RuntimeContextCacheKey,
        builder: F,
    ) -> Option<String>
    where
        F: FnOnce() -> Option<String>,
    {
        let mut guard = self
            .runtime_context_by_key
            .lock()
            .expect("session runtime context cache lock poisoned");
        if let Some(cached) = guard.get(&key) {
            return cached.as_deref().map(str::to_string);
        }
        let value = builder();
        guard.insert(key, value.as_deref().map(Arc::from));
        value
    }
}

pub(super) fn coordinator_simple_mode_enabled() -> bool {
    crate::system_prompt::coordinator_simple_mode_enabled()
}

pub(super) fn build_base_system_for_cache(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
) -> String {
    // The variant hook: coordinator mode selects the coordinator
    // contract exactly as the old hardcoded branch did.
    let variant = crate::system_prompt::PromptVariant::from_context(ctx);
    crate::system_prompt::PromptAssembly::for_variant(&variant).assemble_base(config, ctx)
}

pub(super) fn build_prompt_parts_with_session_cache(
    config: &SystemPromptConfig,
    ctx: &DynamicPromptContext,
    session_prompt_state: &SessionPromptState,
) -> (String, Option<String>, Option<String>, bool) {
    let key = SessionPromptCacheKey::from_config(config, ctx);
    let (base_system, cache_hit) = session_prompt_state
        .get_or_insert_base_system(key.clone(), || build_base_system_for_cache(config, ctx));
    let runtime_key = RuntimeContextCacheKey::from_config(key, config, ctx);
    let runtime_context = session_prompt_state.get_or_insert_runtime_context(runtime_key, || {
        crate::system_prompt::build_stable_runtime_context_block(config, ctx)
    });
    let transient_context =
        crate::system_prompt::build_transient_runtime_context_block(config, ctx);
    (
        base_system.to_string(),
        runtime_context,
        transient_context,
        cache_hit,
    )
}

#[cfg(test)]
mod prefix_stability_tests {
    use super::*;
    use crate::prompt_seat::Rung;

    fn config() -> SystemPromptConfig {
        SystemPromptConfig {
            model: "test-model".into(),
            model_marketing_name: None,
            knowledge_cutoff: None,
            tool_names: vec!["Read".into(), "Edit".into()],
            deferred_tool_names: vec!["WebSearch".into()],
            platform: "win32".into(),
            shell: "bash".into(),
            os_version: "Windows 10".into(),
            language: None,
            auto_continue_background_agents: false,
            normal_system_prompt_override: None,
            minimal_system_prompt_override: None,
            chat_system_prompt_override: None,
        }
    }

    /// What the `memory` plugin puts on the seat: one section on the memory
    /// rung, whose text is document content rather than session identity.
    fn memory_section(text: &str) -> PluginPromptSection {
        PluginPromptSection::new("memory prompt", Rung::Memory, text)
    }

    fn ctx() -> DynamicPromptContext {
        DynamicPromptContext {
            cwd: "F:/work/project".into(),
            is_git: true,
            git_status: Some("Current branch: main".into()),
            rebon_md_content: Some("# rules".into()),
            plugin_prompt_sections: vec![memory_section("remember things")],
            session_date: Some("2026-08-14".into()),
            scratchpad_dir: Some("/tmp/scratch".into()),
            ..DynamicPromptContext::default()
        }
    }

    /// The base key reads the process-global sub-agents switch, which
    /// `query::tests` toggles under the crate's env lock; every test here
    /// that compares two consecutive keys holds the same lock, or a toggle
    /// landing between the two calls reads as a spurious cache miss.
    fn hold_env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    /// Prefix-stability contract: two turns with the same identity produce
    /// byte-identical prompt parts, the second from the frozen cache.
    #[test]
    fn same_session_two_turns_keep_the_prompt_prefix_byte_stable() {
        let _env = hold_env_lock();
        let state = SessionPromptState::default();
        let config = config();
        let ctx = ctx();
        let (b1, r1, t1, hit1) = build_prompt_parts_with_session_cache(&config, &ctx, &state);
        let (b2, r2, t2, hit2) = build_prompt_parts_with_session_cache(&config, &ctx, &state);
        assert!(!hit1, "first turn builds");
        assert!(hit2, "second turn must reuse the frozen base");
        assert_eq!(b1, b2, "base plane must be byte-stable");
        assert_eq!(r1, r2, "stable runtime context must be byte-stable");
        assert_eq!(t1, t2, "transient block is deterministic for equal state");
    }

    /// The freeze contract half: identity-stable CONTENT drift (REBON.md /
    /// memory edited mid-session) must NOT rewrite message index 0.
    #[test]
    fn content_drift_under_a_stable_identity_never_rewrites_the_frozen_block() {
        let _env = hold_env_lock();
        let state = SessionPromptState::default();
        let config = config();
        let ctx1 = ctx();
        let (b1, r1, _t, _) = build_prompt_parts_with_session_cache(&config, &ctx1, &state);

        let mut drifted = ctx1.clone();
        drifted.rebon_md_content = Some("# rules CHANGED underneath".into());
        drifted.plugin_prompt_sections = vec![memory_section("new memory content")];
        let (b2, r2, _t, hit) = build_prompt_parts_with_session_cache(&config, &drifted, &state);
        assert!(hit);
        assert_eq!(b1, b2);
        assert_eq!(
            r1, r2,
            "content inputs are deliberately excluded from the identity key"
        );
    }

    /// Plugin prompt sections are IDENTITY, not content: a composition
    /// registering sections re-renders the block (with the tool shape it
    /// arrives alongside), it never silently mutates the frozen bytes.
    #[test]
    fn plugin_section_registration_is_an_identity_change() {
        let _env = hold_env_lock();
        let state = SessionPromptState::default();
        let config = config();
        let ctx1 = ctx();
        let (_b, r1, _t, _) = build_prompt_parts_with_session_cache(&config, &ctx1, &state);

        // Appended, not replaced: `ctx()` already carries the memory
        // section, and dropping it would be a second change to the plane.
        let mut with_sections = ctx1.clone();
        with_sections
            .plugin_prompt_sections
            .push(PluginPromptSection {
                name: "tool:probe".into(),
                rung: Rung::Context,
                order: 110.0,
                text: "Use the probe tool.".into(),
            });
        let (_b, r2, _t, _) =
            build_prompt_parts_with_session_cache(&config, &with_sections, &state);
        assert_ne!(r1, r2, "a new section set is a new identity, not drift");
        assert!(r2.as_deref().unwrap_or("").contains("Use the probe tool."));

        // And the new identity is itself frozen: repeating it is a hit.
        let (_b, r3, _t, hit) =
            build_prompt_parts_with_session_cache(&config, &with_sections, &state);
        assert!(hit);
        assert_eq!(r2, r3);
    }

    /// A section on a base rung is base-plane IDENTITY: one appearing
    /// mid-session (a plugin loading, or a section gated on a tool that just
    /// arrived) re-renders the frozen base rather than serving stale bytes,
    /// and the stable block — whose rung it is not on — stays as it was.
    #[test]
    fn a_base_rung_section_is_a_base_identity_change() {
        let _env = hold_env_lock();
        let state = SessionPromptState::default();
        let config = config();
        let ctx1 = ctx();
        let (b1, r1, _t, _) = build_prompt_parts_with_session_cache(&config, &ctx1, &state);

        let mut with_style = ctx1.clone();
        with_style
            .plugin_prompt_sections
            .push(PluginPromptSection::new(
                "style",
                Rung::Style,
                "Prefer short answers.",
            ));
        let (b2, r2, _t, hit) = build_prompt_parts_with_session_cache(&config, &with_style, &state);
        assert!(!hit, "a new base section set is a new identity, not a hit");
        assert_ne!(b1, b2);
        assert!(b2.contains("Prefer short answers."));
        assert_eq!(
            r1, r2,
            "a base-rung section is not the stable plane's identity"
        );

        // The new identity is itself frozen: repeating it is a hit.
        let (b3, _r, _t, hit) = build_prompt_parts_with_session_cache(&config, &with_style, &state);
        assert!(hit);
        assert_eq!(b2, b3);
        // And the original identity is still frozen, byte for byte.
        let (b4, _r, _t, hit) = build_prompt_parts_with_session_cache(&config, &ctx1, &state);
        assert!(hit);
        assert_eq!(b1, b4);
    }
}
