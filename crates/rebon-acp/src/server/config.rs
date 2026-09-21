use rebon_proto::types::{ConfigOption, ConfigOptionType, ConfigOptionValue};

/// The rows a *session* answers for, which the `config-options` seat does not
/// hold.
///
/// Everything backed by the config file has moved to that seat, registered by
/// whoever owns the setting: the Core `core-config-options` plugin for rebon's
/// own keys, and each feature plugin for its own. What is left here is session
/// state — the permission mode and the model in force, the pruning this
/// session does — which the seat cannot answer because in `--acp` and `serve`
/// one process holds many sessions and each has its own.
///
/// [`DefaultHandler`](super::handler::DefaultHandler) concatenates the seat's
/// rows onto these, so a plugin's row and rebon's own arrive through one list.
pub(super) fn default_config_options() -> Vec<ConfigOption> {
    vec![
        permissions_config_option(),
        model_config_option(),
        context_prune_config_option(),
        auto_compact_config_option(),
    ]
}

pub(super) fn update_config_options(
    config_options: &[ConfigOption],
    config_id: &str,
    value: &str,
) -> Vec<ConfigOption> {
    let mut updated = config_options.to_vec();
    if let Some(option) = updated.iter_mut().find(|o| o.id == config_id) {
        if matches!(option.option_type, ConfigOptionType::Text)
            || option
                .options
                .iter()
                .any(|candidate| candidate.value == value)
        {
            option.current_value = value.to_string();
        }
    }
    updated
}

pub(super) fn has_config_option_value(
    config_options: &[ConfigOption],
    config_id: &str,
    value: &str,
) -> bool {
    config_options
        .iter()
        .find(|option| option.id == config_id)
        .map(|option| {
            matches!(option.option_type, ConfigOptionType::Text)
                || option
                    .options
                    .iter()
                    .any(|candidate| candidate.value == value)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::default_config_options;

    #[test]
    fn permission_options_keep_wire_values_and_describe_behavior_and_risk() {
        let options = default_config_options();
        let permissions = options
            .iter()
            .find(|option| option.id == "permissions")
            .expect("permissions option must be present");
        let values_and_descriptions: Vec<(&str, &str)> = permissions
            .options
            .iter()
            .map(|option| {
                (
                    option.value.as_str(),
                    option
                        .description
                        .as_deref()
                        .expect("permission options need descriptions"),
                )
            })
            .collect();

        assert_eq!(
            values_and_descriptions,
            vec![
                (
                    "acceptEdits",
                    "Automatically approve file edits; other operations still follow permission rules and may prompt",
                ),
                (
                    "bypassPermissions",
                    "Allow tool use without permission prompts. Highest risk: use only in a trusted, isolated workspace",
                ),
                (
                    "default",
                    "Apply configured allow/deny rules and prompt when an operation needs approval",
                ),
                (
                    "dontAsk",
                    "Never prompt for approval; operations that are not already allowed are denied and may fail",
                ),
                (
                    "plan",
                    "Restrict this session to planning and inspection instead of changes; does not change the startup default",
                ),
                (
                    "auto",
                    "Use the safety classifier to approve low-risk operations automatically; risky or uncertain operations still require confirmation",
                ),
            ]
        );
    }


}

fn permissions_config_option() -> ConfigOption {
    ConfigOption {
        id: "permissions".to_string(),
        name: "Permissions".to_string(),
        description: Some(
            "Choose how Rebon handles tool permission checks and approval prompts for this session"
                .to_string(),
        ),
        category: Some("mode".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "default".to_string(),
        options: vec![
            ConfigOptionValue {
                value: "acceptEdits".to_string(),
                name: "Accept edits".to_string(),
                description: Some(
                    "Automatically approve file edits; other operations still follow permission rules and may prompt"
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "bypassPermissions".to_string(),
                name: "Bypass Permissions".to_string(),
                description: Some(
                    "Allow tool use without permission prompts. Highest risk: use only in a trusted, isolated workspace"
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "default".to_string(),
                name: "Default".to_string(),
                description: Some(
                    "Apply configured allow/deny rules and prompt when an operation needs approval"
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "dontAsk".to_string(),
                name: "Don't Ask".to_string(),
                description: Some(
                    "Never prompt for approval; operations that are not already allowed are denied and may fail"
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "plan".to_string(),
                name: "Plan Mode".to_string(),
                description: Some(
                    "Restrict this session to planning and inspection instead of changes; does not change the startup default"
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "auto".to_string(),
                name: "Auto mode".to_string(),
                description: Some(
                    "Use the safety classifier to approve low-risk operations automatically; risky or uncertain operations still require confirmation"
                        .to_string(),
                ),
            },
        ],
    }
}

fn model_config_option() -> ConfigOption {
    ConfigOption {
        id: "model".to_string(),
        name: "Model".to_string(),
        description: Some("AI model to use".to_string()),
        category: Some("model".to_string()),
        option_type: ConfigOptionType::Text,
        current_value: "default".to_string(),
        options: Vec::new(),
    }
}

fn context_prune_config_option() -> ConfigOption {
    ConfigOption {
        id: "context_prune".to_string(),
        name: "Context Pruning".to_string(),
        description: Some(
            "Automatically prune stale tool results and duplicate tool calls \
             to reduce token usage in long conversations"
                .to_string(),
        ),
        category: Some("optimization".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "conservative".to_string(),
        options: vec![
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some("No pruning — send full context every request".to_string()),
            },
            ConfigOptionValue {
                value: "conservative".to_string(),
                name: "Conservative".to_string(),
                description: Some(
                    "Clear old tool result content (safe, preserves structure)".to_string(),
                ),
            },
            ConfigOptionValue {
                value: "aggressive".to_string(),
                name: "Aggressive".to_string(),
                description: Some(
                    "Conservative + deduplicate identical tool calls + purge failed inputs"
                        .to_string(),
                ),
            },
        ],
    }
}

fn auto_compact_config_option() -> ConfigOption {
    ConfigOption {
        id: "auto_compact".to_string(),
        name: "Auto Compact".to_string(),
        description: Some(
            "Automatically truncate old messages when approaching the context window limit"
                .to_string(),
        ),
        category: Some("optimization".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "on".to_string(),
        options: vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some(
                    "Truncate oldest messages when input tokens exceed threshold".to_string(),
                ),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some(
                    "Never auto-truncate — may hit context window errors".to_string(),
                ),
            },
        ],
    }
}

