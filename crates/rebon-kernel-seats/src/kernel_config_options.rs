//! `core-config-options`: the Core plugin that owns the `config-options` seat
//! and the settings rows rebon itself answers for.
//!
//! `apply` provides the seat on the kernel root and registers the rows whose
//! value is rebon's own `settings.json`. A plugin registers its rows the same
//! way, on its own context, and they leave with it.
//!
//! Only the settings that live in the config file are here. Anything a
//! *session* answers for — the permission mode, the model in force — is
//! registered by whoever holds the session registry, because in `--acp` and
//! `serve` there are many sessions behind one process and the row has to ask
//! about the right one.

use std::sync::Arc;

use crate::kernel_code_mode::PLUGIN_ID as CODE_MODE_PLUGIN_ID;
pub use rebon_config_seat::{
    ConfigOptionProvider, ConfigOptionSpec, ConfigOptionValue, ConfigSeat, ConfigSeatService,
};
use rebon_core::model_routing::MODEL_ROUTING_PLUGIN_ID;
use rebon_kernel::{Context, KernelError, Plugin, PluginMeta, Service};

/// The plugin id, which is also its config key and the name `/plugins` shows.
pub const PLUGIN_ID: &str = "core-config-options";

/// The id the panel and every surface's dispatch name the routing switch by.
pub const MODEL_ROUTING_OPTION: &str = "model_routing";

/// The id the panel and every surface's dispatch name the Code Mode switch by.
pub const CODE_MODE_OPTION: &str = "code_mode";

/// The category experimental features' rows share, which the panel keeps in a
/// group of its own after everything else.
pub const EXPERIMENTAL: &str = "experimental";

/// What a row shows when the setting is absent and rebon has no opinion.
const FOLLOW_THE_MODEL: &str = "auto";

pub struct CoreConfigOptionsPlugin;

impl Plugin for CoreConfigOptionsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).provides(&[<ConfigSeatService as Service>::NAME])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ConfigSeat::new();
        ctx.provide::<ConfigSeatService>(seat.clone())?;
        register_core_options(&seat, ctx)
    }
}

/// The rows backed by `settings.json`, in the order the panel shows them.
pub fn register_core_options(seat: &Arc<ConfigSeat>, ctx: &Context) -> Result<(), KernelError> {
    seat.register(
        ctx,
        ConfigOptionSpec::select("language", "Response language")
            .describe(
                "The language the agent answers in. It is passed to the model as a \
                 preference, and does not translate the terminal interface.",
            )
            .in_category("agent"),
        Arc::new(LanguageOption),
    )?;
    seat.register(
        ctx,
        ConfigOptionSpec::select("shell_tool", "Shell tool")
            .describe(
                "Which shell the agent runs commands through. Bash and PowerShell are \
                 separate tools with their own syntax, permission rules, and prompt \
                 guidance.",
            )
            .in_category("agent"),
        Arc::new(ShellToolOption),
    )?;
    seat.register(
        ctx,
        ConfigOptionSpec::select("fast_mode", "Fast Mode")
            .describe("Use OpenAI service_tier: priority on fast-capable model requests")
            .in_category("optimization"),
        Arc::new(FastModeOption),
    )?;
    seat.register(
        ctx,
        ConfigOptionSpec::select("claude_codex_fallback", "Claude/Codex fallback")
            .describe(
                "Load compatible skills and commands from .claude and .codex \
                 directories. Takes effect after restarting Rebon.",
            )
            .in_category("agent"),
        Arc::new(ClaudeCodexFallbackOption),
    )?;
    // An entry point that withholds a plugin cannot turn it on, so a row
    // offering to would only ever answer with a refusal.
    if !ctx.is_plugin_withheld(MODEL_ROUTING_PLUGIN_ID) {
        seat.register(
            ctx,
            ConfigOptionSpec::select(MODEL_ROUTING_OPTION, "Auto model routing")
                .describe(
                    "Let a classifier pick the provider, model and reasoning effort from each \
                     new session's first prompt. The router rows below configure it; they are \
                     only there while it is on. Sessions already routed keep their model.",
                )
                .in_category(EXPERIMENTAL),
            Arc::new(ModelRoutingSwitch),
        )?;
    }
    if !ctx.is_plugin_withheld(CODE_MODE_PLUGIN_ID) {
        seat.register(
            ctx,
            ConfigOptionSpec::select(CODE_MODE_OPTION, "Code Mode")
                .describe(
                    "Offer `run_code`, which lets the model write one program that chains \
                     tool calls instead of calling them one by one. Permissions still apply \
                     to every call it makes. Needs a trusted Node runtime (`rebon node \
                     install`). The default applies to new sessions; `/codemode on|off` \
                     changes the current one.",
                )
                .in_category(EXPERIMENTAL),
            Arc::new(CodeModeSwitch),
        )?;
    }
    Ok(())
}

/// `on` / `off` as the panel spells them.
fn on_off() -> Vec<ConfigOptionValue> {
    vec![
        ConfigOptionValue {
            value: "on".to_string(),
            name: "On".to_string(),
            description: None,
        },
        ConfigOptionValue {
            value: "off".to_string(),
            name: "Off".to_string(),
            description: None,
        },
    ]
}

/// The response language the agent is asked to answer in.
///
/// The row is the config file's, not a session's: the key is read once per
/// turn when the system prompt is built, so a change is in force on the next
/// turn of every open session without any of them being told.
struct LanguageOption;

impl ConfigOptionProvider for LanguageOption {
    fn current(&self, _session: Option<&str>) -> String {
        rebon_config::saved_language_locale().unwrap_or_else(|| FOLLOW_THE_MODEL.to_string())
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        let mut choices = vec![ConfigOptionValue {
            value: FOLLOW_THE_MODEL.to_string(),
            name: "Auto".to_string(),
            description: Some(
                "Say nothing about language; the model follows the conversation.".to_string(),
            ),
        }];
        for locale in rebon_config::LANGUAGE_LOCALES {
            let (name, description) = match locale {
                "en" => ("English", "Answer in English."),
                "zh-CN" => ("简体中文", "Answer in Chinese."),
                "ja" => ("日本語", "Answer in Japanese."),
                // A locale added to the list without a name here still shows,
                // under its own code, rather than vanishing from the row.
                other => (other, "Answer in this language."),
            };
            choices.push(ConfigOptionValue {
                value: locale.to_string(),
                name: name.to_string(),
                description: Some(description.to_string()),
            });
        }
        choices
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        // `auto` is the absence of the key, so it clears rather than writing
        // the word: `saved_language` only recognises real locales, and a file
        // holding "auto" would read as absent anyway — with the difference
        // that nothing could tell it from a typo.
        let locale = (value != FOLLOW_THE_MODEL).then_some(value);
        rebon_config::save_language(locale)
            .map_err(|error| format!("could not save the language: {error}"))
    }
}

/// Which shell tool the agent is offered.
struct ShellToolOption;

impl ConfigOptionProvider for ShellToolOption {
    fn current(&self, _session: Option<&str>) -> String {
        rebon_config::saved_shell_tool().unwrap_or_else(|| FOLLOW_THE_MODEL.to_string())
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        vec![
            ConfigOptionValue {
                value: "auto".to_string(),
                name: "Auto".to_string(),
                description: Some(
                    "Pick per platform: PowerShell on Windows, Bash elsewhere, and \
                     PowerShell alone when no Git Bash is installed."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "bash".to_string(),
                name: "Bash".to_string(),
                description: Some("Offer the Bash tool only.".to_string()),
            },
            ConfigOptionValue {
                value: "powershell".to_string(),
                name: "PowerShell".to_string(),
                description: Some(
                    "Offer the PowerShell tool only. Requires PowerShell to be installed."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "both".to_string(),
                name: "Both".to_string(),
                description: Some(
                    "Offer both and let the model pick per command. Costs one extra tool \
                     schema per request."
                        .to_string(),
                ),
            },
        ]
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        if !self
            .choices(None)
            .iter()
            .any(|choice| choice.value == value)
        {
            return Err(format!("`{value}` is not a shell tool setting"));
        }
        rebon_config::save_shell_tool(value);
        Ok(())
    }
}

/// OpenAI's priority service tier.
///
/// Persisting is all this row does. Whether the *running* session sends the
/// header is the session's own state, and the surface that owns the session
/// re-reads this on the next request — the same path `/fast` takes.
struct FastModeOption;

impl ConfigOptionProvider for FastModeOption {
    fn current(&self, _session: Option<&str>) -> String {
        if rebon_config::saved_fast_mode_enabled() {
            "on".to_string()
        } else {
            "off".to_string()
        }
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        on_off()
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let enabled = match value {
            "on" => true,
            "off" => false,
            other => return Err(format!("`{other}` is not on or off")),
        };
        rebon_config::save_fast_mode_enabled(enabled)
            .map_err(|error| format!("could not save fast mode: {error}"))
    }
}

/// Skills and commands from `.claude` / `.codex`.
///
/// Read once at startup, so the row says the change waits for a restart
/// rather than pretending the running process will pick it up.
struct ClaudeCodexFallbackOption;

impl ConfigOptionProvider for ClaudeCodexFallbackOption {
    fn current(&self, _session: Option<&str>) -> String {
        if rebon_config::saved_claude_codex_fallback_enabled() {
            "on"
        } else {
            "off"
        }
        .to_string()
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some(
                    "Include user and project .claude/.codex skills and commands after restart."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some(
                    "Only load Rebon and plugin skills and commands after restart.".to_string(),
                ),
            },
        ]
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let enabled = match value {
            "on" => true,
            "off" => false,
            other => return Err(format!("`{other}` is not on or off")),
        };
        rebon_config::save_claude_codex_fallback_enabled(enabled);
        Ok(())
    }
}

/// A feature plugin's `plugins.<id>.enabled` switch, as the settings chain
/// has it — the same read the registry boots and reconciles from — or the
/// plugin's own default when no file sets it.
fn plugin_enabled(id: &str) -> bool {
    rebon_config::saved_plugin_switches()
        .get(id)
        .copied()
        .unwrap_or_else(|| {
            rebon_kernel::process_registry()
                .and_then(|registry| registry.def(id))
                .is_some_and(|def| def.default_enabled)
        })
}

/// Flip a feature plugin's switch the way `/kernel enable|disable` does.
///
/// The registry first, so the next prompt in this process already sees the
/// change; then the file, so it survives a restart. The file's own reconcile
/// then finds nothing left to do.
fn set_plugin_enabled(id: &str, enabled: bool) -> Result<(), String> {
    if let Some(registry) = rebon_kernel::process_registry() {
        let report = registry
            .set_enabled(id, enabled)
            .map_err(|error| error.to_string())?;
        if let Some((_, error)) = report.failed.first() {
            return Err(format!("`{id}` did not load: {error}"));
        }
    }
    rebon_config::save_plugin_enabled(id, enabled)
        .map_err(|error| format!("could not save plugins.{id}.enabled: {error}"))?;
    // The panel writes the user file; a project or local file that sets the
    // switch still wins, and the write above just reconciled back to it.
    if plugin_enabled(id) != enabled {
        return Err(format!(
            "a project settings file sets plugins.{id}.enabled, and it overrides this one"
        ));
    }
    Ok(())
}

/// `plugins.model-routing.enabled`, the only thing that turns routing off.
///
/// Core rather than the routing plugin's own row: switching the plugin off
/// takes its rows with it, and a switch that vanishes once used could never
/// be switched back on from the panel.
struct ModelRoutingSwitch;

impl ConfigOptionProvider for ModelRoutingSwitch {
    fn current(&self, _session: Option<&str>) -> String {
        if plugin_enabled(MODEL_ROUTING_PLUGIN_ID) {
            "on"
        } else {
            "off"
        }
        .to_string()
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some(
                    "Route each new session's first prompt. Needs a router backend configured."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some("Every session stays on the model it starts with.".to_string()),
            },
        ]
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        let enabled = match value {
            "on" => true,
            "off" => false,
            other => return Err(format!("`{other}` is not on or off")),
        };
        set_plugin_enabled(MODEL_ROUTING_PLUGIN_ID, enabled)
    }
}

/// Code Mode takes two switches to reach a session: `plugins.code-mode.enabled`
/// makes `run_code` available at all, and `defaultOn` starts new sessions with
/// it on rather than waiting for `/codemode on`. The row folds them into the
/// three states they can actually be in, so neither has to be edited by hand.
///
/// Core for the same reason as [`ModelRoutingSwitch`]: the switch loads the
/// plugin, and the plugin cannot carry the row that turns it back on.
struct CodeModeSwitch;

impl CodeModeSwitch {
    fn default_on() -> bool {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        crate::kernel_code_mode::default_on_in(&rebon_config::config_home_dir(), &cwd)
    }

    fn save_default_on(on: bool) -> Result<(), String> {
        let mut patch = serde_json::Map::new();
        patch.insert(
            crate::kernel_code_mode::DEFAULT_ON_SETTING.to_string(),
            serde_json::Value::Bool(on),
        );
        rebon_config::save_plugin_settings_in_dir(
            &rebon_config::config_home_dir(),
            CODE_MODE_PLUGIN_ID,
            &patch,
        )
        .map_err(|error| format!("could not save the Code Mode default: {error}"))?;
        if Self::default_on() != on {
            return Err(format!(
                "a project settings file sets plugins.{CODE_MODE_PLUGIN_ID}.{}, and it \
                 overrides this one",
                crate::kernel_code_mode::DEFAULT_ON_SETTING
            ));
        }
        Ok(())
    }
}

impl ConfigOptionProvider for CodeModeSwitch {
    fn current(&self, _session: Option<&str>) -> String {
        match (plugin_enabled(CODE_MODE_PLUGIN_ID), Self::default_on()) {
            (false, _) => "off",
            (true, false) => "manual",
            (true, true) => "on",
        }
        .to_string()
    }

    fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
        vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some(
                    "New sessions start with run_code on; /codemode off turns it off for one."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "manual".to_string(),
                name: "On request".to_string(),
                description: Some(
                    "Available, but each session starts with it off until /codemode on."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some(
                    "No run_code anywhere, including sessions that had turned it on.".to_string(),
                ),
            },
        ]
    }

    fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
        match value {
            // The default before the plugin: a session the reconcile builds
            // should already find the default it is meant to start with.
            "on" | "manual" => {
                Self::save_default_on(value == "on")?;
                set_plugin_enabled(CODE_MODE_PLUGIN_ID, true)
            }
            // `defaultOn` is left as it is: switching the plugin back on
            // later returns to whichever of the two it was.
            "off" => set_plugin_enabled(CODE_MODE_PLUGIN_ID, false),
            other => Err(format!("`{other}` is not on, manual or off")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    fn seat_with_core_options() -> (Arc<Kernel>, Arc<ConfigSeat>) {
        let kernel = Kernel::new();
        let seat = ConfigSeat::new();
        register_core_options(&seat, kernel.context()).expect("registers");
        (kernel, seat)
    }

    #[test]
    fn the_core_rows_are_registered_in_panel_order() {
        let (_kernel, seat) = seat_with_core_options();
        assert_eq!(
            seat.options(None)
                .iter()
                .map(|option| option.id.clone())
                .collect::<Vec<_>>(),
            vec![
                "language",
                "shell_tool",
                "fast_mode",
                "claude_codex_fallback",
                MODEL_ROUTING_OPTION,
                CODE_MODE_OPTION
            ]
        );
    }

    /// The panel keeps these in a group of their own, after everything else.
    #[test]
    fn the_experimental_switches_share_the_experimental_category() {
        let (_kernel, seat) = seat_with_core_options();
        for id in [MODEL_ROUTING_OPTION, CODE_MODE_OPTION] {
            let row = seat
                .options(None)
                .into_iter()
                .find(|option| option.id == id)
                .expect("registered");
            assert_eq!(row.category.as_deref(), Some(EXPERIMENTAL), "{id}");
        }
    }

    /// One row for the two Code Mode switches, in the three states they can
    /// be in together.
    #[test]
    fn the_code_mode_row_drives_the_plugin_switch_and_the_session_default() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("code-mode-switch");
        let (_kernel, seat) = seat_with_core_options();
        let current = || {
            seat.options(None)
                .into_iter()
                .find(|option| option.id == CODE_MODE_OPTION)
                .expect("registered")
                .current_value
        };
        let switch = || {
            rebon_config::saved_plugin_switches()
                .get(CODE_MODE_PLUGIN_ID)
                .copied()
        };
        assert_eq!(current(), "off", "the plugin is off by default");

        seat.apply(None, CODE_MODE_OPTION, "manual")
            .expect("available");
        assert_eq!(current(), "manual");
        assert_eq!(switch(), Some(true));
        assert!(!CodeModeSwitch::default_on());

        seat.apply(None, CODE_MODE_OPTION, "on").expect("on");
        assert_eq!(current(), "on");
        assert!(CodeModeSwitch::default_on());

        seat.apply(None, CODE_MODE_OPTION, "off").expect("off");
        assert_eq!(current(), "off");
        assert_eq!(switch(), Some(false));
        assert!(
            CodeModeSwitch::default_on(),
            "off leaves the default for when it comes back on"
        );

        let err = seat
            .apply(None, CODE_MODE_OPTION, "auto")
            .expect_err("not a state");
        assert!(err.contains("auto"), "{err}");
    }

    /// The panel's only way to turn routing off: the router rows configure
    /// it, and none of them offers "off" — they leave with the plugin.
    #[test]
    fn the_routing_row_flips_the_plugin_switch_both_ways() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("routing-switch");
        let (_kernel, seat) = seat_with_core_options();
        let current = || {
            seat.options(None)
                .into_iter()
                .find(|option| option.id == MODEL_ROUTING_OPTION)
                .expect("registered")
                .current_value
        };
        assert_eq!(current(), "off", "the plugin is off by default");

        seat.apply(None, MODEL_ROUTING_OPTION, "on")
            .expect("turns on");
        assert_eq!(current(), "on");
        assert_eq!(
            rebon_config::saved_plugin_switches().get(MODEL_ROUTING_PLUGIN_ID),
            Some(&true)
        );

        seat.apply(None, MODEL_ROUTING_OPTION, "off")
            .expect("turns off");
        assert_eq!(current(), "off");
        assert_eq!(
            rebon_config::saved_plugin_switches().get(MODEL_ROUTING_PLUGIN_ID),
            Some(&false),
            "written as an explicit switch, not left to the default"
        );
    }

    /// `rebon exec` and `--acp` withhold routing and Code Mode; offering a
    /// switch there would only ever be refused.
    #[test]
    fn a_surface_that_withholds_the_plugins_gets_no_switch_rows() {
        struct Inert;
        impl Plugin for Inert {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new(MODEL_ROUTING_PLUGIN_ID)
            }
            fn apply(&self, _ctx: &Context) -> Result<(), KernelError> {
                Ok(())
            }
        }
        let kernel = Kernel::new();
        let registry = rebon_kernel::PluginRegistry::new(
            kernel.clone(),
            &[
                rebon_kernel::PluginDef {
                    id: MODEL_ROUTING_PLUGIN_ID,
                    title: "routing",
                    kind: rebon_kernel::PluginKind::Feature,
                    default_enabled: false,
                    factory: |_| Ok(Box::new(Inert)),
                },
                crate::kernel_code_mode::PLUGIN,
            ],
            rebon_kernel::PluginHost {
                kernel: kernel.clone(),
                config_dir: std::env::temp_dir(),
            },
        );
        registry
            .withhold(&[MODEL_ROUTING_PLUGIN_ID, CODE_MODE_PLUGIN_ID])
            .unwrap();

        let seat = ConfigSeat::new();
        register_core_options(&seat, kernel.context()).expect("registers");
        assert!(!seat.has(MODEL_ROUTING_OPTION));
        assert!(!seat.has(CODE_MODE_OPTION));
        assert!(seat.has("language"), "the other rows are still there");
    }

    /// The row the panel was missing, and the thing it actually controls.
    #[test]
    fn the_language_row_offers_auto_plus_every_locale_the_prompt_reader_knows() {
        let (_kernel, seat) = seat_with_core_options();
        let language = seat
            .options(None)
            .into_iter()
            .find(|option| option.id == "language")
            .expect("the language row is registered");

        let values: Vec<String> = language
            .options
            .iter()
            .map(|choice| choice.value.clone())
            .collect();
        assert_eq!(values[0], "auto", "absence comes first");
        for locale in rebon_config::LANGUAGE_LOCALES {
            assert!(
                values.contains(&locale.to_string()),
                "{locale} in {values:?}"
            );
        }
        assert!(
            language
                .description
                .as_deref()
                .is_some_and(|text| text.contains("does not translate")),
            "the row has to say it is not a UI language: {:?}",
            language.description
        );
    }

    /// The wording `rebon-acp`'s test used to pin: the setting does nothing
    /// until the process restarts, and the row has to say so.
    #[test]
    fn the_claude_codex_row_says_the_change_waits_for_a_restart() {
        let (_kernel, seat) = seat_with_core_options();
        let row = seat
            .options(None)
            .into_iter()
            .find(|option| option.id == "claude_codex_fallback")
            .expect("registered");
        assert!(
            row.description
                .as_deref()
                .is_some_and(|text| text.contains("restarting Rebon")),
            "{:?}",
            row.description
        );
        assert_eq!(
            row.options
                .iter()
                .map(|value| value.value.as_str())
                .collect::<Vec<_>>(),
            vec!["on", "off"]
        );
    }

    #[test]
    fn a_value_outside_the_choices_is_refused_rather_than_written() {
        let (_kernel, seat) = seat_with_core_options();
        let err = seat
            .apply(None, "shell_tool", "fish")
            .expect_err("not a shell tool setting");
        assert!(err.contains("fish"), "{err}");

        let err = seat
            .apply(None, "fast_mode", "maybe")
            .expect_err("not on or off");
        assert!(err.contains("maybe"), "{err}");

        let err = seat
            .apply(None, MODEL_ROUTING_OPTION, "auto")
            .expect_err("not on or off");
        assert!(err.contains("auto"), "{err}");
    }

    #[test]
    fn disposing_the_plugin_context_takes_every_core_row_with_it() {
        let kernel = Kernel::new();
        let seat = ConfigSeat::new();
        let scope = kernel.context().fork(PLUGIN_ID);
        register_core_options(&seat, &scope).expect("registers");
        assert_eq!(seat.len(), 6);

        scope.dispose();
        assert!(
            seat.is_empty(),
            "a Core row must not outlive the plugin that registered it"
        );
    }
}
