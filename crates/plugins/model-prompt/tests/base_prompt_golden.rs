//! The base plane with this plugin loaded matches the approved wording in
//! the golden fixture, preserving the layout from before the sections moved.
//!
//! The sections come off a real seat through a real load of
//! [`rebon_plugin_model_prompt::PLUGIN`], so the golden also pins which rung
//! and order the plugin registers at, and that the switch removes them.

use std::sync::Arc;

use rebon_core::prompt_seat::{sections_for, PromptSeat, PromptSeatService, PromptSubject};
use rebon_core::system_prompt::{
    DynamicPromptContext, PromptAssembly, PromptVariant, SystemPromptConfig,
};
use rebon_kernel::{
    Context, DesiredSet, Kernel, KernelError, Plugin, PluginDef, PluginHost, PluginKind,
    PluginMeta, PluginRegistry,
};
use rebon_plugin_model_prompt::{sections, PLUGIN, PLUGIN_ID, RUN_CODE_TOOL};

const FROZEN: &str = include_str!("fixtures/base-prompt-2026-09-05.txt");

struct SeatPlugin;

impl Plugin for SeatPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("test-seat").provides(&[rebon_core::prompt_seat::PROMPT_SEAT_SERVICE])
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

fn config(model: &str) -> SystemPromptConfig {
    SystemPromptConfig {
        model: model.into(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        tool_names: vec!["Read".into(), "Edit".into(), "Bash".into()],
        deferred_tool_names: vec!["WebSearch".into()],
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

/// The base plane for `model`, with whatever the seat under `kernel`
/// contributes for it.
fn base_for(kernel: &Kernel, model: &str, tools: &[&str]) -> String {
    let session = kernel.context().fork_scoped("session/golden");
    let subject = PromptSubject::new(model)
        .with_tools(tools.iter().map(|t| t.to_string()).collect(), Vec::new());
    let ctx = DynamicPromptContext {
        plugin_prompt_sections: sections_for(&session, &subject),
        ..DynamicPromptContext::default()
    };
    PromptAssembly::for_variant(&PromptVariant::Standard).assemble_base(&config(model), &ctx)
}

/// Where the two moved sections start inside the frozen plane.
fn moved_sections_start() -> usize {
    FROZEN
        .find("\n\n# Tone and style\n")
        .expect("the frozen plane carries the tone section")
}

#[test]
fn the_fixture_preserves_the_base_plane_layout() {
    assert!(
        !FROZEN.contains('\r'),
        "the fixture must be LF (see .gitattributes)"
    );
    assert!(FROZEN.contains("\n\n# Tone and style\n"));
    assert!(FROZEN.contains("\n\n# Output efficiency\n"));
    assert!(
        !FROZEN.ends_with('\n'),
        "planes join without a trailing newline"
    );
}

#[test]
fn base_plane_retains_safety_and_protocol_literals() {
    for literal in [
        "<system-reminder>",
        "<user-prompt-submit-hook>",
        "methodName",
        "method_name",
        "OWASP top 10",
        "C2 frameworks",
        "DoS attacks",
        "rm -rf",
        "git reset --hard",
        "--no-verify",
        "REBON.md",
        "ToolSearch",
        "InvokeDeferredTool",
        "/help",
        "file_path:line_number",
        "owner/repo#123",
        "_vars",
        "// removed",
    ] {
        assert!(FROZEN.contains(literal), "missing literal: {literal}");
    }
    for requirement in [
        "Decline requests involving destructive techniques",
        "an explicit authorization context",
        "NEVER invent or infer URLs",
        "After a denial, never repeat that identical tool invocation",
        "tell the user explicitly before you continue",
        "One approval, such as permission for a git push, is NOT blanket approval",
        "Permission extends only to its stated scope",
        "Never invoke a deferred tool before retrieving its schema",
        "hide or weaken failing tests, lints, or type checks",
        "Code and tool calls are exempt from this guidance",
    ] {
        assert!(FROZEN.contains(requirement), "missing rule: {requirement}");
    }
}

/// Every model gets the approved base plane byte for byte.
///
/// The models named here sit at opposite ends of the capability table —
/// `tier::model_tier` reads the first two as frontier and the rest not —
/// and the assertion is that this makes no difference to the bytes.
#[test]
fn every_model_reproduces_the_frozen_base_byte_for_byte() {
    let (kernel, _registry) = boot();
    for model in [
        "claude-opus-5",
        "gpt-5.6-sol",
        "gpt-5.6-luna",
        "deepseek-v4-pro",
        "gpt-4o",
        "some-self-hosted-thing",
    ] {
        assert_eq!(base_for(&kernel, model, &["Read"]), FROZEN, "{model}");
        assert_eq!(base_for(&kernel, model, &[]), FROZEN, "{model}, no tools");
    }
}

/// Astra's plane is the frozen one plus its addendum at the very end —
/// after the efficiency section, nothing between — with the batching line
/// present exactly when the turn offers `run_code`.
#[test]
fn astra_gets_the_base_plus_the_working_addendum_at_the_end() {
    let (kernel, _registry) = boot();
    let base = FROZEN.to_string();
    let without = base_for(&kernel, "gpt-6-astra", &["Read"]);
    assert_eq!(
        without,
        format!("{base}\n\n{}", sections::astra_working(false))
    );
    let with = base_for(&kernel, "gpt-6-astra", &["Read", RUN_CODE_TOOL]);
    assert_eq!(with, format!("{base}\n\n{}", sections::astra_working(true)));
    assert!(!without.contains("`run_code`"));
    assert!(with.contains("Combine independent reads or searches in a single `run_code` program"));
    for prompt in [&without, &with] {
        assert!(prompt.contains(
            "In one line between tool calls, explain your current action and its purpose"
        ));
        assert!(prompt.contains(
            "what changed, what you checked and how you checked it, and what remains to be done"
        ));
        assert!(prompt.contains("a conflict between the user's request and a skill file or REBON.md instruction causes you to stop"));
        assert!(prompt.contains("identify that file and quote the sentence responsible"));
    }
    // Order inside the plane: tone, then efficiency, then the addendum.
    let tone = with.find("\n\n# Tone and style\n").unwrap();
    let efficiency = with.find("\n\n# Output efficiency\n").unwrap();
    let addendum = with.find("\n\n# Working in this session\n").unwrap();
    assert!(tone < efficiency && efficiency < addendum);
}

/// With the plugin switched off every model loses the two sections, and
/// nothing else in the plane moves.
#[test]
fn switching_the_plugin_off_ends_the_base_at_the_tool_discovery_section() {
    let (kernel, registry) = boot();
    registry
        .set_enabled(PLUGIN_ID, false)
        .expect("model-prompt is a feature plugin");
    let expected = &FROZEN[..moved_sections_start()];
    let last_header = expected
        .rfind("\n# ")
        .map(|at| &expected[at + 1..])
        .unwrap_or_default();
    assert!(
        last_header.starts_with("# Using your tools"),
        "the plane ends with the tool-discovery section: {last_header:?}"
    );
    for model in ["claude-opus-5", "gpt-5.6-sol", "gpt-6-astra"] {
        assert_eq!(base_for(&kernel, model, &["Read"]), expected, "{model}");
    }
}
