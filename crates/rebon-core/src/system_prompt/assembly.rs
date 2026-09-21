//! The system-prompt assembly registry.
//!
//! One declarative section table drives what used to be three hand-rolled
//! builder functions (`build_base_system_prompt`,
//! `build_stable_runtime_context_block`,
//! `build_transient_runtime_context_block`). Each section is (id, rank,
//! build): ordering is explicit rank, never insertion order (the seat
//! discipline), a build returning `None` drops the section, and every
//! plane joins its surviving sections with `"\n\n"` exactly as before —
//! the migration is byte-identical by construction and pinned by the
//! `legacy` reference tests below.
//!
//! [`PromptVariant`] is the per-assembly variant hook: a variant selects
//! the section table before assembly. Its one consumer is the coordinator
//! contract (previously a hardcoded branch in `build_split_system_prompt`).
//! Per-model preferences are not a variant: they are sections a plugin puts
//! on the prompt seat (`rebon-plugin-model-prompt` holds the tone and
//! efficiency sections at `Rung::Style` / `Rung::Efficiency`).
//!
//! Determinism contract (prefix stability): assembly output is a pure
//! function of `(variant, config, ctx)` — no timestamps, no map-iteration
//! order, no environment reads inside builds. The one environment input
//! (`REBON_SIMPLE`) is resolved when the VARIANT is constructed, at the
//! same call sites that read it before.
//!
//! Plugin sections. `ctx.plugin_prompt_sections` is the turn's snapshot of
//! the kernel's `prompt-sections` seat ([`crate::prompt_seat`]). Each
//! section names a [`Rung`], which is a plane plus a rank in the same
//! numbering as the tables below, so a plane assembles by merging its own
//! table with the sections on its rungs by rank alone — an engine section
//! and a plugin section on the same rank render engine first. An empty
//! snapshot changes nothing: the assembly stays byte-identical to the
//! pre-seat output.

use super::sections::{
    actions_section, doing_tasks_section, env_info_section, intro_section, language_section,
    runtime_tools_section, scratchpad_section, session_specific_guidance_section,
    stable_tool_discovery_section, system_section,
};
use super::{DynamicPromptContext, SystemPromptConfig, TOOL_RESULT_RETENTION_REMINDER};
use crate::prompt_seat::{sort_sections, PluginPromptSection};

type SectionBuild = fn(&SystemPromptConfig, &DynamicPromptContext) -> Option<String>;

/// One registered prompt section: explicit rank + a pure build function.
struct PromptSectionDef {
    id: &'static str,
    rank: u16,
    build: SectionBuild,
}

const fn section(id: &'static str, rank: u16, build: SectionBuild) -> PromptSectionDef {
    PromptSectionDef { id, rank, build }
}

/// Which prompt shape this assembly produces. The variant hook: resolved
/// once per assembly (coordinator state from the turn context, provider
/// profiles later), it selects the whole section table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptVariant {
    /// The standard main-agent prompt.
    Standard,
    /// The coordinator contract replaces the base plane; runtime context
    /// planes are shared with the standard variant.
    Coordinator { simple: bool, use_worktree: bool },
}

impl PromptVariant {
    /// Resolve the variant for one turn — the exact decision
    /// `build_split_system_prompt` used to hardcode, including the
    /// `REBON_SIMPLE` read at this point.
    pub fn from_context(ctx: &DynamicPromptContext) -> Self {
        if ctx.coordinator_mode {
            PromptVariant::Coordinator {
                simple: coordinator_simple_mode_enabled(),
                use_worktree: ctx.coordinator_use_worktree,
            }
        } else {
            PromptVariant::Standard
        }
    }
}

/// Whether `REBON_SIMPLE` asks for the trimmed coordinator contract. The
/// single home for a check that used to be duplicated in
/// `system_prompt.rs` and `query/session_prompt.rs`.
pub fn coordinator_simple_mode_enabled() -> bool {
    rebon_types::env::env_truthy("REBON_SIMPLE")
}

/// The assembly registry for one variant: three planes of ranked sections.
pub struct PromptAssembly {
    base: Vec<PromptSectionDef>,
    stable: Vec<PromptSectionDef>,
    transient: Vec<PromptSectionDef>,
    /// Whether plugin sections on base rungs render into this variant's
    /// base plane. The standard persona is made of the sections the base
    /// rungs name; the coordinator contract is one opaque section that
    /// never carried them, and takes none.
    base_accepts_plugin_sections: bool,
}

impl PromptAssembly {
    /// The section table for `variant`. Tables are validated (unique ids,
    /// strictly ascending ranks) so ordering bugs fail loudly in tests
    /// rather than silently reordering the prompt.
    pub fn for_variant(variant: &PromptVariant) -> Self {
        let assembly = match variant {
            PromptVariant::Standard => Self {
                base: standard_base_sections(),
                stable: stable_context_sections(),
                transient: transient_context_sections(),
                base_accepts_plugin_sections: true,
            },
            PromptVariant::Coordinator {
                simple,
                use_worktree,
            } => Self {
                base: coordinator_base_sections(*simple, *use_worktree),
                stable: stable_context_sections(),
                transient: transient_context_sections(),
                base_accepts_plugin_sections: false,
            },
        };
        debug_assert!(assembly.tables_are_well_formed());
        assembly
    }

    fn tables_are_well_formed(&self) -> bool {
        [&self.base, &self.stable, &self.transient]
            .into_iter()
            .all(|plane| {
                plane.windows(2).all(|w| w[0].rank < w[1].rank)
                    && plane
                        .iter()
                        .enumerate()
                        .all(|(i, s)| plane[..i].iter().all(|prior| prior.id != s.id))
            })
    }

    /// One plane's surviving sections in render order: the engine's table
    /// merged with the plugin sections whose rung is on `which`, by rank.
    /// At equal rank the engine section renders first; plugin sections on
    /// one rank keep their seat order (position inside the rung, then name).
    fn assemble_plane(
        plane: &[PromptSectionDef],
        config: &SystemPromptConfig,
        ctx: &DynamicPromptContext,
        plugin_sections: &[PluginPromptSection],
        which: PromptPlane,
    ) -> Vec<String> {
        // (rank, engine-before-plugin, arrival) — the arrival index keeps
        // the pre-sorted plugin order under the stable sort.
        let mut items: Vec<(u16, u8, usize, String)> = plane
            .iter()
            .enumerate()
            .filter_map(|(arrival, section)| {
                (section.build)(config, ctx).map(|content| (section.rank, 0, arrival, content))
            })
            .filter(|(_, _, _, content)| !content.is_empty())
            .collect();
        let mut plugins: Vec<PluginPromptSection> = plugin_sections
            .iter()
            .filter(|section| section.rung.plane() == which && !section.text.is_empty())
            .cloned()
            .collect();
        sort_sections(&mut plugins);
        items.extend(
            plugins
                .into_iter()
                .enumerate()
                .map(|(arrival, section)| (section.rung.rank(), 1, arrival, section.text)),
        );
        items.sort_by_key(|(rank, kind, arrival, _)| (*rank, *kind, *arrival));
        items
            .into_iter()
            .map(|(_, _, _, content)| content)
            .collect()
    }

    /// The stable base prompt (top-level provider `system` field).
    pub fn assemble_base(&self, config: &SystemPromptConfig, ctx: &DynamicPromptContext) -> String {
        let plugin_sections: &[PluginPromptSection] = if self.base_accepts_plugin_sections {
            &ctx.plugin_prompt_sections
        } else {
            &[]
        };
        Self::assemble_plane(&self.base, config, ctx, plugin_sections, PromptPlane::Base)
            .join("\n\n")
    }

    /// Low-churn runtime context; `None` when every section is absent.
    pub fn assemble_stable(
        &self,
        config: &SystemPromptConfig,
        ctx: &DynamicPromptContext,
    ) -> Option<String> {
        let sections = Self::assemble_plane(
            &self.stable,
            config,
            ctx,
            &ctx.plugin_prompt_sections,
            PromptPlane::Stable,
        );
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    }

    /// High-churn per-request context; `None` when every section is absent.
    pub fn assemble_transient(
        &self,
        config: &SystemPromptConfig,
        ctx: &DynamicPromptContext,
    ) -> Option<String> {
        let sections = Self::assemble_plane(
            &self.transient,
            config,
            ctx,
            &ctx.plugin_prompt_sections,
            PromptPlane::Transient,
        );
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    }

    /// Section ids of one plane in assembly order (diagnostics/conformance).
    pub fn plane_ids(&self, plane: PromptPlane) -> Vec<&'static str> {
        let plane = match plane {
            PromptPlane::Base => &self.base,
            PromptPlane::Stable => &self.stable,
            PromptPlane::Transient => &self.transient,
        };
        plane.iter().map(|s| s.id).collect()
    }
}

/// The three assembly planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPlane {
    Base,
    Stable,
    Transient,
}

fn optional_non_empty(value: &Option<String>) -> Option<String> {
    value.as_ref().filter(|value| !value.is_empty()).cloned()
}

fn standard_base_sections() -> Vec<PromptSectionDef> {
    vec![
        section("intro", 10, |_, _| Some(intro_section())),
        section("system", 20, |_, _| Some(system_section())),
        section("doing-tasks", 30, |_, _| Some(doing_tasks_section(None))),
        section("actions", 40, |_, _| Some(actions_section())),
        section("tool-discovery", 50, |_, _| {
            Some(stable_tool_discovery_section())
        }),
        // Ranks 60 and 70 — tone-and-style and output-efficiency — are
        // `Rung::Style` / `Rung::Efficiency` on the prompt seat, held by
        // `rebon-plugin-model-prompt`.
    ]
}

fn coordinator_base_sections(simple: bool, use_worktree: bool) -> Vec<PromptSectionDef> {
    // The contract is one opaque section; `simple`/`use_worktree` are baked
    // into the closure-free build via a rank-stable pair of const-like
    // variants (fn pointers cannot capture, so the two flags select among
    // four prebuilt builds).
    let build: SectionBuild = match (simple, use_worktree) {
        (false, false) => |_, _| {
            Some(crate::coordinator_mode::coordinator_system_prompt_with_options(false, false))
        },
        (false, true) => |_, _| {
            Some(crate::coordinator_mode::coordinator_system_prompt_with_options(false, true))
        },
        (true, false) => |_, _| {
            Some(crate::coordinator_mode::coordinator_system_prompt_with_options(true, false))
        },
        (true, true) => {
            |_, _| Some(crate::coordinator_mode::coordinator_system_prompt_with_options(true, true))
        }
    };
    vec![section("coordinator-contract", 10, build)]
}

fn stable_context_sections() -> Vec<PromptSectionDef> {
    vec![
        section("env-info", 10, |config, ctx| {
            Some(env_info_section(config, ctx))
        }),
        section("language", 20, |_, ctx| {
            ctx.language.as_ref().map(|lang| language_section(lang))
        }),
        section("rebon-md", 30, |_, ctx| {
            optional_non_empty(&ctx.rebon_md_content)
        }),
        // Rank 40 is `Rung::Memory`: the auto-`MEMORY.md` section is
        // `rebon-plugin-memory`'s, off the prompt seat.
        // Nothing else stands here, so with that plugin off the plane closes
        // up and the project instruction files run straight into the MCP
        // instructions — which is what "auto-memory is off" has always meant.
        section("mcp-instructions", 50, |_, ctx| {
            optional_non_empty(&ctx.mcp_instructions)
        }),
        // Tool names are low-churn request context, not immutable state. If
        // the engine registry or active filter changes, this section
        // intentionally changes with the request shape and may reset
        // provider/prefix caches.
        section("runtime-tools", 60, |config, _| {
            (!config.tool_names.is_empty() || !config.deferred_tool_names.is_empty())
                .then(|| runtime_tools_section(&config.tool_names, &config.deferred_tool_names))
        }),
        section("session-guidance", 70, |config, _| {
            session_specific_guidance_section(
                &config.tool_names,
                &config.deferred_tool_names,
                config.auto_continue_background_agents,
            )
        }),
        // Rank 75 is `Rung::Context`: plugin sections from the seat land
        // here, next to rebon's own tool guidance, merged in by
        // `assemble_plane`.
        section("scratchpad", 80, |_, ctx| {
            ctx.scratchpad_dir
                .as_ref()
                .filter(|dir| !dir.is_empty())
                .map(|dir| scratchpad_section(dir))
        }),
        section("retention-reminder", 90, |_, _| {
            Some(TOOL_RESULT_RETENTION_REMINDER.to_string())
        }),
    ]
}

fn transient_context_sections() -> Vec<PromptSectionDef> {
    vec![
        section("worker-tools", 10, |_, ctx| {
            optional_non_empty(&ctx.worker_tools_context)
        }),
        section("git-status", 20, |_, ctx| {
            optional_non_empty(&ctx.git_status).map(|git| format!("gitStatus: {git}"))
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_seat::Rung;

    /// The reference builders the registry is compared against: the section
    /// order and join rules written out plainly, without the registry, so the
    /// assembly has something to reproduce byte for byte (coordinator env read
    /// parameterized so tests never mutate the environment).
    ///
    /// Two sections are not in this reference. Tone and style and output
    /// efficiency belong to `rebon-plugin-model-prompt`, whose
    /// `tests/base_prompt_golden.rs` pins the full seven-section base plane,
    /// and the auto-memory section belongs to `rebon-plugin-memory`, whose
    /// `tests/stable_prompt_golden.rs` pins the stable plane.
    mod legacy {
        use super::super::super::sections::{
            actions_section, doing_tasks_section, env_info_section, intro_section,
            language_section, runtime_tools_section, scratchpad_section,
            session_specific_guidance_section, stable_tool_discovery_section, system_section,
        };
        use super::super::super::{
            DynamicPromptContext, SystemPromptConfig, TOOL_RESULT_RETENTION_REMINDER,
        };

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

        pub fn build_base_system_prompt(_config: &SystemPromptConfig) -> String {
            let mut sections: Vec<String> = Vec::new();
            sections.push(intro_section());
            sections.push(system_section());
            sections.push(doing_tasks_section(None));
            sections.push(actions_section());
            sections.push(stable_tool_discovery_section());
            sections.join("\n\n")
        }

        pub fn coordinator_base_system_prompt(simple: bool, use_worktree: bool) -> String {
            crate::coordinator_mode::coordinator_system_prompt_with_options(simple, use_worktree)
        }

        pub fn build_stable_runtime_context_block(
            config: &SystemPromptConfig,
            ctx: &DynamicPromptContext,
        ) -> Option<String> {
            let mut sections: Vec<String> = Vec::new();
            sections.push(env_info_section(config, ctx));
            if let Some(ref lang) = ctx.language {
                sections.push(language_section(lang));
            }
            if let Some(content) = optional_non_empty_section(&ctx.rebon_md_content) {
                sections.push(content);
            }
            if let Some(mcp) = optional_non_empty_section(&ctx.mcp_instructions) {
                sections.push(mcp);
            }
            if !config.tool_names.is_empty() || !config.deferred_tool_names.is_empty() {
                sections.push(runtime_tools_section(
                    &config.tool_names,
                    &config.deferred_tool_names,
                ));
            }
            if let Some(section) = session_specific_guidance_section(
                &config.tool_names,
                &config.deferred_tool_names,
                config.auto_continue_background_agents,
            ) {
                sections.push(section);
            }
            if let Some(ref dir) = ctx.scratchpad_dir {
                if !dir.is_empty() {
                    sections.push(scratchpad_section(dir));
                }
            }
            sections.push(TOOL_RESULT_RETENTION_REMINDER.to_string());
            finish_sections(sections)
        }

        pub fn build_transient_runtime_context_block(
            _config: &SystemPromptConfig,
            ctx: &DynamicPromptContext,
        ) -> Option<String> {
            let mut sections: Vec<String> = Vec::new();
            if let Some(worker_ctx) = optional_non_empty_section(&ctx.worker_tools_context) {
                sections.push(worker_ctx);
            }
            if let Some(git) = optional_non_empty_section(&ctx.git_status) {
                sections.push(format!("gitStatus: {git}"));
            }
            finish_sections(sections)
        }
    }

    fn minimal_config() -> SystemPromptConfig {
        SystemPromptConfig {
            model: "test-model".into(),
            model_marketing_name: None,
            knowledge_cutoff: None,
            tool_names: Vec::new(),
            deferred_tool_names: Vec::new(),
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

    fn full_config() -> SystemPromptConfig {
        SystemPromptConfig {
            model: "claude-opus-4-6".into(),
            model_marketing_name: Some("Claude Opus 4.6".into()),
            knowledge_cutoff: Some("May 2025".into()),
            tool_names: vec!["Read".into(), "Edit".into(), "Bash".into(), "Skill".into()],
            deferred_tool_names: vec!["WebSearch".into(), "NotebookEdit".into()],
            platform: "linux".into(),
            shell: "zsh".into(),
            os_version: "Ubuntu 24.04".into(),
            language: Some("Chinese".into()),
            auto_continue_background_agents: true,
            normal_system_prompt_override: None,
            minimal_system_prompt_override: None,
            chat_system_prompt_override: None,
        }
    }

    fn empty_ctx() -> DynamicPromptContext {
        DynamicPromptContext {
            cwd: "F:/work".into(),
            ..DynamicPromptContext::default()
        }
    }

    fn full_ctx() -> DynamicPromptContext {
        DynamicPromptContext {
            cwd: "/home/dev/project".into(),
            is_git: true,
            git_status: Some("Current branch: main\n\nStatus:\n M src/lib.rs".into()),
            language: Some("Chinese".into()),
            rebon_md_content: Some("# Project rules\n- keep tests green".into()),
            mcp_instructions: Some("## some-mcp\nUse it sparingly.".into()),
            session_date: Some("2026-08-14".into()),
            coordinator_mode: false,
            coordinator_use_worktree: false,
            worker_tools_context: Some("Worker tools: Read, Edit".into()),
            scratchpad_dir: Some("/tmp/scratch".into()),
            plugin_prompt_sections: Vec::new(),
        }
    }

    #[test]
    fn deferred_monitor_adds_monitor_loop_and_agent_guidance() {
        let mut config = minimal_config();
        config.deferred_tool_names = vec!["Monitor".into()];
        let stable = PromptAssembly::for_variant(&PromptVariant::Standard)
            .assemble_stable(&config, &empty_ctx())
            .unwrap();

        assert!(stable.contains(
            "Monitor is for selective event streams from external commands or WebSockets"
        ));
        assert!(stable.contains("Never wait for Agent completion through Monitor"));
        assert!(stable.contains("Coarse recurring full prompts belong in `/loop`"));
    }

    /// Plugin sections render inside the stable plane between
    /// session-guidance and scratchpad, sorted by (order, name); empty
    /// texts drop; an empty list leaves the plane byte-identical.
    #[test]
    fn plugin_sections_render_sorted_inside_the_stable_plane() {
        let config = full_config();
        let mut ctx = full_ctx();
        let assembly = PromptAssembly::for_variant(&PromptVariant::Standard);
        let without = assembly.assemble_stable(&config, &ctx).unwrap();

        ctx.plugin_prompt_sections = vec![
            PluginPromptSection {
                name: "tool:web_fetch".into(),
                rung: Rung::Context,
                order: 111.0,
                text: "Use the web_fetch tool.".into(),
            },
            PluginPromptSection {
                name: "tool:web_search".into(),
                rung: Rung::Context,
                order: 110.0,
                text: "Use the web_search tool.".into(),
            },
            PluginPromptSection {
                name: "empty".into(),
                rung: Rung::Context,
                order: 1.0,
                text: String::new(),
            },
        ];
        let with = assembly.assemble_stable(&config, &ctx).unwrap();
        assert_ne!(without, with);
        let expected_block = "Use the web_search tool.\n\nUse the web_fetch tool.";
        assert!(with.contains(expected_block), "sorted by order: {with}");
        // Position: after the earlier stable sections, before scratchpad.
        let scratchpad = scratchpad_section("/tmp/scratch");
        let block_at = with.find(expected_block).unwrap();
        let scratchpad_at = with.find(&scratchpad).unwrap();
        assert!(
            block_at < scratchpad_at,
            "plugin sections precede scratchpad"
        );
        // Everything else is untouched: removing the block (plus joiner)
        // restores the original bytes.
        let restored = with.replace(&format!("{expected_block}\n\n"), "");
        assert_eq!(restored, without, "plugin block is a clean insertion");
    }

    /// Every (config, ctx) cell the golden matrix covers. Empty-string
    /// fields exercise the drop-if-empty rules alongside the None cells.
    fn matrix() -> Vec<(SystemPromptConfig, DynamicPromptContext)> {
        let mut degenerate = full_ctx();
        degenerate.rebon_md_content = Some(String::new());
        degenerate.scratchpad_dir = Some(String::new());
        degenerate.worker_tools_context = Some(String::new());
        degenerate.git_status = None;
        vec![
            (minimal_config(), empty_ctx()),
            (minimal_config(), full_ctx()),
            (full_config(), empty_ctx()),
            (full_config(), full_ctx()),
            (full_config(), degenerate),
        ]
    }

    /// Golden: the registry reproduces the legacy builders byte for
    /// byte across the whole matrix, on all three planes.
    #[test]
    fn registry_assembly_is_byte_identical_to_the_legacy_builders() {
        let assembly = PromptAssembly::for_variant(&PromptVariant::Standard);
        for (config, ctx) in matrix() {
            assert_eq!(
                assembly.assemble_base(&config, &ctx),
                legacy::build_base_system_prompt(&config),
                "base plane diverged"
            );
            assert_eq!(
                assembly.assemble_stable(&config, &ctx),
                legacy::build_stable_runtime_context_block(&config, &ctx),
                "stable plane diverged"
            );
            assert_eq!(
                assembly.assemble_transient(&config, &ctx),
                legacy::build_transient_runtime_context_block(&config, &ctx),
                "transient plane diverged"
            );
        }
    }

    /// The coordinator variant reproduces the legacy hardcoded branch for
    /// every (simple, use_worktree) combination, and shares the runtime
    /// context planes with the standard variant.
    #[test]
    fn coordinator_variant_is_byte_identical_to_the_legacy_branch() {
        let config = full_config();
        let ctx = full_ctx();
        for simple in [false, true] {
            for use_worktree in [false, true] {
                let assembly = PromptAssembly::for_variant(&PromptVariant::Coordinator {
                    simple,
                    use_worktree,
                });
                assert_eq!(
                    assembly.assemble_base(&config, &ctx),
                    legacy::coordinator_base_system_prompt(simple, use_worktree),
                    "coordinator base diverged (simple={simple}, worktree={use_worktree})"
                );
                assert_eq!(
                    assembly.assemble_stable(&config, &ctx),
                    legacy::build_stable_runtime_context_block(&config, &ctx),
                    "coordinator stable plane diverged"
                );
            }
        }
    }

    /// Prefix stability: same inputs, same bytes — across repeated calls
    /// AND across independently constructed assemblies.
    #[test]
    fn assembly_is_deterministic_across_calls_and_instances() {
        let config = full_config();
        let ctx = full_ctx();
        let a = PromptAssembly::for_variant(&PromptVariant::Standard);
        let b = PromptAssembly::for_variant(&PromptVariant::Standard);
        assert_eq!(
            a.assemble_base(&config, &ctx),
            a.assemble_base(&config, &ctx)
        );
        assert_eq!(
            a.assemble_base(&config, &ctx),
            b.assemble_base(&config, &ctx)
        );
        assert_eq!(
            a.assemble_stable(&config, &ctx),
            b.assemble_stable(&config, &ctx)
        );
        assert_eq!(
            a.assemble_transient(&config, &ctx),
            b.assemble_transient(&config, &ctx)
        );
    }

    /// The registered section order is part of the contract — a reorder is
    /// a prompt change and must fail a test, not slip through.
    #[test]
    fn section_tables_are_pinned() {
        let standard = PromptAssembly::for_variant(&PromptVariant::Standard);
        assert_eq!(
            standard.plane_ids(PromptPlane::Base),
            [
                "intro",
                "system",
                "doing-tasks",
                "actions",
                "tool-discovery"
            ]
        );
        assert_eq!(
            standard.plane_ids(PromptPlane::Stable),
            [
                "env-info",
                "language",
                "rebon-md",
                // Rank 40 is the memory plugin's `Rung::Memory`.
                "mcp-instructions",
                "runtime-tools",
                "session-guidance",
                "scratchpad",
                "retention-reminder",
            ]
        );
        assert_eq!(
            standard.plane_ids(PromptPlane::Transient),
            ["worker-tools", "git-status"]
        );
        let coordinator = PromptAssembly::for_variant(&PromptVariant::Coordinator {
            simple: false,
            use_worktree: false,
        });
        assert_eq!(
            coordinator.plane_ids(PromptPlane::Base),
            ["coordinator-contract"]
        );
    }

    /// A section on a base rung renders on the base plane at its rank —
    /// after the engine section holding the same rank when there is one,
    /// in rank order among the plugin sections when there is not — and
    /// nowhere else.
    #[test]
    fn base_rung_sections_render_at_their_rank_on_the_base_plane() {
        let config = full_config();
        let mut ctx = full_ctx();
        let assembly = PromptAssembly::for_variant(&PromptVariant::Standard);
        let base_without = assembly.assemble_base(&config, &ctx);
        let stable_without = assembly.assemble_stable(&config, &ctx);

        ctx.plugin_prompt_sections = vec![
            PluginPromptSection::new("efficiency", Rung::Efficiency, "EFFICIENCY-PLUGIN"),
            PluginPromptSection::new("style", Rung::Style, "STYLE-PLUGIN"),
            PluginPromptSection::new("task", Rung::Task, "TASK-PLUGIN"),
        ];
        let with = assembly.assemble_base(&config, &ctx);
        // Rank 30 has an engine section (doing-tasks): the plugin section
        // follows it. Ranks 60 and 70 have none: the two render after
        // tool-discovery (50), in rank order.
        let expected_middle = format!("{}\n\nTASK-PLUGIN\n\n", doing_tasks_section(None));
        assert!(with.contains(&expected_middle), "{with}");
        let expected_tail = format!(
            "{}\n\nSTYLE-PLUGIN\n\nEFFICIENCY-PLUGIN",
            stable_tool_discovery_section()
        );
        assert!(with.ends_with(&expected_tail), "{with}");
        let restored = with
            .replace("\n\nTASK-PLUGIN", "")
            .replace("\n\nSTYLE-PLUGIN", "")
            .replace("\n\nEFFICIENCY-PLUGIN", "");
        assert_eq!(
            restored, base_without,
            "base-rung sections are a clean insertion"
        );
        assert_eq!(
            assembly.assemble_stable(&config, &ctx),
            stable_without,
            "the stable plane does not see base-rung sections"
        );
    }

    /// The base plane is a function of the table alone: the same bytes for
    /// every config, which is what lets one frozen copy stand as the golden
    /// for the plugin that took two of its sections.
    #[test]
    fn the_base_plane_does_not_depend_on_the_config() {
        let assembly = PromptAssembly::for_variant(&PromptVariant::Standard);
        assert_eq!(
            assembly.assemble_base(&full_config(), &full_ctx()),
            assembly.assemble_base(&minimal_config(), &empty_ctx())
        );
    }

    /// The model-prompt plugin's golden fixture, vouched for from this side:
    /// its first five sections are what the reference base builder produces,
    /// and what follows is exactly the two texts the plugin registers. The
    /// plugin's own golden assembles the plane through the seat; this one
    /// never touches the seat, so the two cannot be wrong together.
    #[test]
    fn the_model_prompt_fixture_is_the_legacy_base_plus_the_plugins_two_texts() {
        const FROZEN: &str =
            include_str!("../../../plugins/model-prompt/tests/fixtures/base-prompt-2026-09-05.txt");
        let head = legacy::build_base_system_prompt(&full_config());
        assert!(
            FROZEN.starts_with(&head),
            "the fixture opens with the legacy base"
        );
        assert_eq!(
            &FROZEN[head.len()..],
            format!(
                "\n\n{}\n\n{}",
                rebon_plugin_model_prompt::sections::TONE_AND_STYLE,
                rebon_plugin_model_prompt::sections::OUTPUT_EFFICIENCY
            )
        );
    }

    /// The coordinator contract is opaque: base-rung sections do not render
    /// into it. The runtime planes still take theirs.
    #[test]
    fn coordinator_base_ignores_base_rung_sections() {
        let config = full_config();
        let mut ctx = full_ctx();
        ctx.coordinator_mode = true;
        let assembly = PromptAssembly::for_variant(&PromptVariant::Coordinator {
            simple: false,
            use_worktree: false,
        });
        let without = assembly.assemble_base(&config, &ctx);

        ctx.plugin_prompt_sections = vec![
            PluginPromptSection::new("style", Rung::Style, "STYLE-PLUGIN"),
            PluginPromptSection::new("ctx", Rung::Context, "CONTEXT-PLUGIN"),
        ];
        assert_eq!(assembly.assemble_base(&config, &ctx), without);
        assert!(assembly
            .assemble_stable(&config, &ctx)
            .unwrap()
            .contains("CONTEXT-PLUGIN"));
    }

    /// A section on the transient rung renders on the transient plane, after
    /// the git status — and is the whole plane when the engine has nothing
    /// transient to say.
    #[test]
    fn transient_rung_sections_land_after_the_git_status() {
        let config = full_config();
        let mut ctx = full_ctx();
        let assembly = PromptAssembly::for_variant(&PromptVariant::Standard);
        let without = assembly.assemble_transient(&config, &ctx).unwrap();

        ctx.plugin_prompt_sections = vec![PluginPromptSection::new(
            "t",
            Rung::Transient,
            "TRANSIENT-PLUGIN",
        )];
        let with = assembly.assemble_transient(&config, &ctx).unwrap();
        assert_eq!(with, format!("{without}\n\nTRANSIENT-PLUGIN"));
        assert!(!assembly
            .assemble_stable(&config, &ctx)
            .unwrap()
            .contains("TRANSIENT-PLUGIN"));

        let mut bare = empty_ctx();
        bare.plugin_prompt_sections = ctx.plugin_prompt_sections.clone();
        assert_eq!(
            assembly.assemble_transient(&config, &bare).as_deref(),
            Some("TRANSIENT-PLUGIN")
        );
    }
}
