//! Per-turn thinking configuration derived from the effort level.
//!
//! Pure arithmetic over the effort ordinal and the provider kind; the
//! headless runner, the background worker and the terminal all ask it
//! the same question before a turn.

use rebon_types::{effort_indicator::EffortProviderKind, ReasoningEffort};

use crate::EngineSession;

pub use rebon_api::effort::{resolve_thinking_from_effort, ThinkingOverrides};

/// Parsed result of an `/effort` command.
pub enum EffortCommand {
    /// Bare `/effort`; the interactive runner opens the picker while
    /// non-interactive callers can still render the current level.
    Show,
    /// `/effort max|xhigh|high|medium|low` — set a specific level.
    Set(ReasoningEffort),
    /// `/effort auto` — reset to auto (model default).
    Auto,
    /// Invalid argument.
    Invalid(String),
}

/// Parsed result of a `/fast` command.
pub enum FastCommand {
    Status,
    On,
    Off,
    Invalid(String),
}

/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn execute_effort_command(
    current: &mut Option<ReasoningEffort>,
    kind: EffortProviderKind,
    cmd: EffortCommand,
) -> String {
    let label = kind.label();
    match cmd {
        EffortCommand::Show => match *current {
            Some(level) => format!("Current {} level: {}", label, level.as_str()),
            None => format!("Current {} level: auto (model default)", label),
        },
        EffortCommand::Set(level) => {
            *current = Some(level);
            format!("Set {} level to {}", label, level.as_str())
        }
        EffortCommand::Auto => {
            *current = None;
            format!("Reset {} level to auto (model default)", label)
        }
        EffortCommand::Invalid(arg) => {
            format!(
                "Unknown {} level '{}'. Usage: /effort [max|xhigh|high|medium|low|auto]",
                label, arg
            )
        }
    }
}

pub fn load_persisted_effort_level(current: &mut Option<ReasoningEffort>) {
    *current = crate::rebon_config::saved_effort_level()
        .as_deref()
        .and_then(effort_level_from_wire);
}

pub fn execute_persisted_effort_command(
    current: &mut Option<ReasoningEffort>,
    kind: EffortProviderKind,
    cmd: EffortCommand,
) -> String {
    let should_persist = matches!(&cmd, EffortCommand::Set(_) | EffortCommand::Auto);
    let output = execute_effort_command(current, kind, cmd);
    if should_persist {
        if let Err(err) =
            crate::rebon_config::save_effort_level(current.map(|level| level.as_str()))
        {
            tracing::warn!(error = %err, "failed to persist effort level to user settings");
        }
    }
    output
}

/// Parse the wire spelling of an effort level.
///
/// `host::runtime_fields::effort_level_from_wire` reads the same five strings
/// and returns `anyhow::Result` instead, treating an unknown value as a
/// misconfigured job where this one reads it as "not set". The table is shared
/// now ([`ReasoningEffort::from_wire_exact`]); the two readings of a value that
/// is not in it are still different, and merging *those* would be a change of
/// behaviour rather than a move.
pub fn effort_level_from_wire(value: &str) -> Option<ReasoningEffort> {
    ReasoningEffort::from_wire_exact(value)
}

pub fn execute_fast_command(session: &EngineSession, cmd: FastCommand) -> Result<String, String> {
    match cmd {
        FastCommand::Status => Ok(format_fast_status(session)),
        FastCommand::On => set_fast_mode(session, true),
        FastCommand::Off => set_fast_mode(session, false),
        FastCommand::Invalid(arg) => Err(format!(
            "Unknown fast mode argument '{}'. Usage: /fast [on|off|status]",
            arg
        )),
    }
}

pub fn set_fast_mode(session: &EngineSession, enabled: bool) -> Result<String, String> {
    session.model.service_tier.set_fast(enabled);
    let value = if enabled { "on" } else { "off" };
    session
        .engine_half
        .handler
        .apply_config_option_local(&session.session_id, "fast_mode", value);
    let persist_result = crate::rebon_config::save_fast_mode_enabled(enabled);
    let persist_failed = persist_result.is_err();
    let persisted = match persist_result {
        Ok(()) => String::new(),
        Err(err) => format!("\nPersist warning: failed to write config: {err}"),
    };
    if !session.model.service_tier_available {
        return Err(format!(
            "Fast mode {} in config, but it is unavailable for current provider `{}`. It only applies to the ChatGPT Codex OAuth backend or api.openai.com providers, not custom providers.{persisted}",
            if enabled { "enabled" } else { "disabled" },
            session.model.provider_name,
        ));
    }
    let text = format!(
        "Fast mode {}. Requests will {} service_tier: \"priority\" from the next request.\n{}{persisted}",
        if enabled { "enabled" } else { "disabled" },
        if enabled { "send" } else { "not send" },
        fast_model_note(session),
    );
    if persist_failed {
        Err(text)
    } else {
        Ok(text)
    }
}

fn format_fast_status(session: &EngineSession) -> String {
    if !session.model.service_tier_available {
        return format_fast_unavailable(session);
    }
    format!(
        "Fast mode: {}\nAvailable: yes (ChatGPT Codex OAuth/api.openai.com provider `{}`)\n{}",
        if session.model.service_tier.is_fast() {
            "on"
        } else {
            "off"
        },
        session.model.provider_name,
        fast_model_note(session),
    )
}

/// What the *model* does with the tier, which is a different question from
/// what the provider does with it.
///
/// The provider line above only says the endpoint would carry the field.
/// Whether the selected model accepts it is per-model, and until this line
/// existed a user had no way to find out: the request either quietly
/// carried a field the model rejects, or quietly did not.
fn fast_model_note(session: &EngineSession) -> String {
    use rebon_api::model_table::{service_tier_support, TierSupport};

    let model = session.model.name.as_str();
    let tier = rebon_api::FAST_SERVICE_TIER;
    match service_tier_support(None, model, tier) {
        TierSupport::Supported => {
            format!("Model `{model}`: accepts service_tier: \"{tier}\".")
        }
        TierSupport::Unsupported => format!(
            "Model `{model}`: does not accept service_tier: \"{tier}\" — it is left out of requests while this model is selected, so fast mode costs nothing here."
        ),
        TierSupport::Unknown => format!(
            "Model `{model}`: not in the model table, so the field is sent as configured. Run /model refresh to update the table."
        ),
    }
}

fn format_fast_unavailable(session: &EngineSession) -> String {
    format!(
        "Fast mode: {} (configured)\nAvailable: no — current provider `{}` is not the ChatGPT Codex OAuth backend or api.openai.com provider. Fast mode does not apply to custom providers.",
        if session.model.service_tier.is_fast() {
            "on"
        } else {
            "off"
        },
        session.model.provider_name
    )
}

pub fn provider_kind_from_format(
    format: crate::rebon_config::ProviderFormat,
) -> EffortProviderKind {
    match format {
        crate::rebon_config::ProviderFormat::Anthropic => EffortProviderKind::Anthropic,
        crate::rebon_config::ProviderFormat::Openai
        | crate::rebon_config::ProviderFormat::OpenaiResponses => EffortProviderKind::OpenAi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_format_recognizes_openai() {
        assert_eq!(
            provider_kind_from_format(crate::rebon_config::ProviderFormat::Openai),
            EffortProviderKind::OpenAi
        );
    }

    #[test]
    fn provider_kind_format_uses_configured_provider_wire_format() {
        use rebon_types::effort_indicator::EffortProviderKind;
        assert_eq!(
            provider_kind_from_format(crate::rebon_config::ProviderFormat::Openai),
            EffortProviderKind::OpenAi
        );
        assert_eq!(
            provider_kind_from_format(crate::rebon_config::ProviderFormat::OpenaiResponses),
            EffortProviderKind::OpenAi
        );
        assert_eq!(
            provider_kind_from_format(crate::rebon_config::ProviderFormat::Anthropic),
            EffortProviderKind::Anthropic
        );
    }
}
