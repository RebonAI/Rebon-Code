//! `model-prompt`: the feature plugin that owns the model-preference
//! sections of the system prompt.
//!
//! Two sections of the base plane are preferences about how a model should
//! talk rather than facts about the session — `# Tone and style` and
//! `# Output efficiency`. This plugin puts them on the kernel's
//! `prompt-sections` seat ([`rebon_core::prompt_seat`]) at [`Rung::Style`]
//! and [`Rung::Efficiency`], the ranks the engine's base table leaves for
//! them, and the engine assembles them there.
//!
//! **Why a plugin.** Different models do not benefit equally from one
//! prompt, and a preference that varies by model has no business as an
//! `if` in the engine. The seat asks this provider per turn with a
//! [`PromptSubject`], so the answer can depend on the model and on the
//! tools the turn offers, and the engine never learns a model's name.
//!
//! **What the engine keeps.** `sub_agent_notes_section` (the spawner appends
//! it to a sub-agent's prompt through a different path) and the coordinator
//! contract (which never carries these two sections).
//!
//! **Families.** The two shared sections go to every model unchanged, byte
//! for byte, which is what `tests/base_prompt_golden.rs` pins. GPT-6 Astra
//! additionally gets a short addendum
//! ([`sections::astra_working`]) after the efficiency section, and its one
//! tool-specific line is only issued when the turn actually offers
//! `run_code`. Which family a model is in is asked of the vendor catalogue
//! in exactly one place, [`model_family`]; nothing else in the tree spells
//! a model's name for this purpose.
//!
//! **Switching it off.** `plugins.model-prompt.enabled = false` takes every
//! section here out of the prompt for *every* model: the base plane then
//! ends at the tool-discovery section. That is the ordinary meaning of a
//! feature plugin's switch, not a per-model override — a user who wants a
//! different persona writes the `normal` system-prompt override, which
//! replaces the whole base plane, plugin sections included.

use std::sync::Arc;

use rebon_api::vendor::ProviderVendor;
use rebon_core::prompt_seat::{
    PluginPromptSection, PromptSeatService, PromptSectionProvider, PromptSubject, Rung,
    PROMPT_SEAT_SERVICE,
};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod sections;
pub mod tier;

/// Stable id: the config key `plugins.model-prompt.enabled`.
pub const PLUGIN_ID: &str = "model-prompt";

const PROVIDER_ID: &str = "model-prompt";

/// Section name of the `# Tone and style` contribution.
pub const TONE_AND_STYLE_SECTION: &str = "model-prompt/tone-and-style";
/// Section name of the `# Output efficiency` contribution.
pub const OUTPUT_EFFICIENCY_SECTION: &str = "model-prompt/output-efficiency";
/// Section name of the Astra addendum.
pub const ASTRA_WORKING_SECTION: &str = "model-prompt/astra-working";

/// The Code Mode tool, by the name the turn's projection lists it under.
///
/// The same string as `rebon_kernel_seats::kernel_code_mode::RUN_CODE_TOOL_NAME`
/// — a drift test there keeps them equal — spelled here because the
/// harness depends on this crate, not the other way round.
pub const RUN_CODE_TOOL: &str = "run_code";

/// The catalogue id of GPT-6 Astra, the one model with a family of its own.
const ASTRA_CATALOGUE_ID: &str = "gpt-6-astra";

/// Which prompt profile a model gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    /// The two shared sections and nothing else. Claude, GPT-5.6 and every
    /// model the plugin does not single out.
    Default,
    /// GPT-6 Astra: the shared sections plus the working addendum.
    Astra,
}

/// The family for `model` — the plugin's single question to the vendor
/// catalogue. The lookup is the catalogue's own (case-insensitive, a dated
/// snapshot suffix resolves to its row), so `gpt-6-astra-20260903` is Astra
/// and an id the catalogue does not know is `Default`.
pub fn model_family(model: &str) -> ModelFamily {
    match ProviderVendor::OpenAi.known_model(model) {
        Some(row) if row.id == ASTRA_CATALOGUE_ID => ModelFamily::Astra,
        _ => ModelFamily::Default,
    }
}

/// The sections this plugin contributes for one turn's subject, in render
/// order: the two shared sections, then the Astra addendum for that family.
pub fn sections_for_subject(subject: &PromptSubject) -> Vec<PluginPromptSection> {
    // Both sections go to every model unchanged.
    let mut sections = vec![
        PluginPromptSection::new(
            TONE_AND_STYLE_SECTION,
            Rung::Style,
            sections::TONE_AND_STYLE,
        ),
        PluginPromptSection::new(
            OUTPUT_EFFICIENCY_SECTION,
            Rung::Efficiency,
            sections::OUTPUT_EFFICIENCY,
        ),
    ];
    if model_family(&subject.model) == ModelFamily::Astra {
        sections.push(
            PluginPromptSection::new(
                ASTRA_WORKING_SECTION,
                Rung::Efficiency,
                sections::astra_working(subject.has_tool(RUN_CODE_TOOL)),
            )
            // After the shared efficiency section on the same rung.
            .with_order(1.0),
        );
    }
    sections
}

/// The provider on the seat.
pub struct ModelPromptProvider;

impl PromptSectionProvider for ModelPromptProvider {
    fn sections_for(&self, subject: &PromptSubject) -> Vec<PluginPromptSection> {
        sections_for_subject(subject)
    }
}

#[derive(Default)]
pub struct ModelPromptPlugin;

impl Plugin for ModelPromptPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[PROMPT_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        // `require`, not `get`: a missing seat is a failed load the registry
        // reports, not a prompt that quietly lost two sections.
        let seat = ctx.require::<PromptSeatService>()?;
        seat.register(ctx, PROVIDER_ID, Arc::new(ModelPromptProvider))
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(ModelPromptPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Model-preference prompt sections (tone and style, output efficiency)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::prompt_seat::{sections_for, PromptSeat};
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};

    /// Stands in for `core-tools`, which lives in `rebon-harness` and cannot
    /// be depended on from here. All this plugin needs is the one seat.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[PROMPT_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<PromptSeatService>(PromptSeat::new())
        }
    }

    fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(SeatPlugin))
    }

    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "test-seat",
            title: "Test seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_seat,
        },
        PLUGIN,
    ];

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    fn subject() -> PromptSubject {
        PromptSubject::new("claude-opus-5").with_tools(vec!["Read".into()], vec![])
    }

    /// The two sections are on the seat at the rungs the engine's rows held,
    /// and the switch takes them off and puts them back.
    #[test]
    fn the_switch_takes_the_sections_off_the_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let session = kernel.context().fork_scoped("session/abc");

        let sections = sections_for(&session, &subject());
        assert_eq!(
            sections
                .iter()
                .map(|s| (s.name.as_str(), s.rung))
                .collect::<Vec<_>>(),
            vec![
                (TONE_AND_STYLE_SECTION, Rung::Style),
                (OUTPUT_EFFICIENCY_SECTION, Rung::Efficiency),
            ]
        );
        assert_eq!(sections[0].text, sections::TONE_AND_STYLE);
        assert_eq!(sections[1].text, sections::OUTPUT_EFFICIENCY);
        // Capability does not change the text. The two models sit at
        // opposite ends of the tier table and get the same bytes.
        let small = PromptSubject::new("gpt-5.6-luna").with_tools(vec!["Read".into()], vec![]);
        assert_eq!(
            sections_for(&session, &small)[1].text,
            sections::OUTPUT_EFFICIENCY
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("model-prompt is a feature plugin");
        assert!(sections_for(&session, &subject()).is_empty());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(sections_for(&session, &subject()).len(), 2);
    }

    /// The family table has two rows, and the catalogue decides which one
    /// a model is in: exact id, any case, or a dated snapshot of it.
    #[test]
    fn the_catalogue_decides_the_family() {
        for model in [
            "claude-opus-5",
            "claude-fable-5",
            "gpt-5.6-sol",
            "gpt-5.5",
            "deepseek-v4-pro",
            "not-a-model",
            "",
        ] {
            assert_eq!(model_family(model), ModelFamily::Default, "{model}");
        }
        for model in ["gpt-6-astra", "GPT-6-Astra", "gpt-6-astra-20260903"] {
            assert_eq!(model_family(model), ModelFamily::Astra, "{model}");
        }
    }

    /// Astra gets the addendum on the efficiency rung after the shared
    /// section, and the batching line only when the turn offers `run_code`
    /// — eagerly or through tool search.
    #[test]
    fn astra_gets_the_addendum_and_the_batching_line_only_with_run_code() {
        let (kernel, _registry) = boot();
        let session = kernel.context().fork_scoped("session/abc");

        let bare = PromptSubject::new("gpt-6-astra").with_tools(vec!["Read".into()], vec![]);
        let sections = sections_for(&session, &bare);
        assert_eq!(sections.len(), 3);
        let addendum = &sections[2];
        assert_eq!(addendum.name, ASTRA_WORKING_SECTION);
        assert_eq!(addendum.rung, Rung::Efficiency);
        assert!(
            addendum.order > sections[1].order,
            "after the shared section"
        );
        assert_eq!(addendum.text, sections::astra_working(false));
        assert!(!addendum.text.contains("run_code"));
        assert!(addendum.text.contains("quote the sentence responsible"));

        let eager = PromptSubject::new("gpt-6-astra")
            .with_tools(vec!["Read".into(), RUN_CODE_TOOL.into()], vec![]);
        let deferred =
            PromptSubject::new("gpt-6-astra").with_tools(vec![], vec![RUN_CODE_TOOL.into()]);
        for subject in [eager, deferred] {
            let text = &sections_for(&session, &subject)[2].text;
            assert_eq!(text, &sections::astra_working(true));
            assert!(text.contains("a single `run_code` program"));
        }

        // Nobody else gets it, run_code or not.
        let other =
            PromptSubject::new("gpt-5.6-sol").with_tools(vec![RUN_CODE_TOOL.into()], vec![]);
        assert_eq!(sections_for(&session, &other).len(), 2);
    }

    /// Loading without the seat is a failed load, reported, not a silent
    /// prompt with two sections missing.
    #[test]
    fn loading_without_the_seat_fails_loudly() {
        static ALONE: &[PluginDef] = &[PLUGIN];
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), ALONE, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(
            report.failed.iter().any(|(id, _)| id == PLUGIN_ID),
            "{:?}",
            report.failed
        );
    }
}
