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

pub use rebon_config_seat::{
    ConfigOptionProvider, ConfigOptionSpec, ConfigOptionValue, ConfigSeat, ConfigSeatService,
};
use rebon_kernel::{Context, KernelError, Plugin, PluginMeta, Service};

/// The plugin id, which is also its config key and the name `/plugins` shows.
pub const PLUGIN_ID: &str = "core-config-options";

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
                "claude_codex_fallback"
            ]
        );
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
    }

    #[test]
    fn disposing_the_plugin_context_takes_every_core_row_with_it() {
        let kernel = Kernel::new();
        let seat = ConfigSeat::new();
        let scope = kernel.context().fork(PLUGIN_ID);
        register_core_options(&seat, &scope).expect("registers");
        assert_eq!(seat.len(), 4);

        scope.dispose();
        assert!(
            seat.is_empty(),
            "a Core row must not outlive the plugin that registered it"
        );
    }
}
