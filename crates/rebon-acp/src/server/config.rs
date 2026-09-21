use rebon_proto::types::{ConfigOption, ConfigOptionType, ConfigOptionValue};

pub(super) fn default_config_options() -> Vec<ConfigOption> {
    vec![
        permissions_config_option(),
        model_config_option(),
        context_prune_config_option(),
        auto_compact_config_option(),
        fast_mode_config_option(),
        update_auto_install_config_option(),
        sub_agents_config_option(),
        shell_tool_config_option(),
        claude_codex_fallback_config_option(),
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

    #[test]
    fn claude_codex_fallback_defaults_off_and_requires_restart() {
        let options = default_config_options();
        let fallback = options
            .iter()
            .find(|option| option.id == "claude_codex_fallback")
            .expect("Claude/Codex fallback option must be present");

        assert_eq!(fallback.current_value, "off");
        assert_eq!(
            fallback
                .options
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            vec!["on", "off"]
        );
        assert!(fallback
            .description
            .as_deref()
            .is_some_and(|description| description.contains("restarting Rebon")));
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

fn fast_mode_config_option() -> ConfigOption {
    ConfigOption {
        id: "fast_mode".to_string(),
        name: "Fast Mode".to_string(),
        description: Some(
            "Use OpenAI service_tier: priority on fast-capable model requests".to_string(),
        ),
        category: Some("optimization".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "off".to_string(),
        options: vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some("Send service_tier: priority from the next request".to_string()),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some("Do not send service_tier: priority".to_string()),
            },
        ],
    }
}

fn update_auto_install_config_option() -> ConfigOption {
    ConfigOption {
        id: "update_auto_install".to_string(),
        name: "Auto install updates".to_string(),
        description: Some(
            "Stores the auto-install preference; register the per-user background runner explicitly with `rebon update service install`. Package installation waits for installer support."
                .to_string(),
        ),
        category: Some("updates".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "off".to_string(),
        options: vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some(
                    "Store the preference only; install the background runner explicitly."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some("Do not allow automatic update installation".to_string()),
            },
        ],
    }
}

fn sub_agents_config_option() -> ConfigOption {
    // Delegation to sub-agents. The actual toggle lives in
    // `rebon_tool::agent::SUB_AGENTS_ENABLED` (process-global
    // atomic) and `~/.rebon/config.json` (persistence). When
    // `off`, the engine filters `AgentTool` out of `tool_names`
    // and `system_prompt::agent_tool_section` is suppressed.
    //
    // We default to "on" here; the CLI runner overrides the
    // `current_value` at startup from the persisted setting via
    // `DefaultHandler::seed_config_option_value`.
    ConfigOption {
        id: "sub_agents".to_string(),
        name: "Sub-agents".to_string(),
        description: Some(
            "Let the main agent delegate to specialized sub-agents (Explore, Plan, etc.) \
             via the Agent tool. When off, the Agent tool is hidden and the system prompt \
             drops the delegation guidance."
                .to_string(),
        ),
        category: Some("agent".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "on".to_string(),
        options: vec![
            ConfigOptionValue {
                value: "on".to_string(),
                name: "On".to_string(),
                description: Some(
                    "Advertise the Agent tool; include sub-agent delegation guidance \
                     in the system prompt."
                        .to_string(),
                ),
            },
            ConfigOptionValue {
                value: "off".to_string(),
                name: "Off".to_string(),
                description: Some(
                    "Hide the Agent tool and omit the delegation guidance section.".to_string(),
                ),
            },
        ],
    }
}

fn shell_tool_config_option() -> ConfigOption {
    // Which shell tool the model is offered. The live switch is
    // `rebon_tool::set_shell_tool_preference` (a process-global atomic
    // read by `BashTool::is_enabled` / `PowerShellTool::is_enabled`), and
    // `~/.rebon/config.json`'s `shellTool` key persists it. Changing it
    // takes effect on the next turn: the disabled shell drops out of both
    // the API tool list and the system prompt's tool section.
    //
    // Defaults to "auto"; the CLI runner overrides `current_value` at
    // startup from the persisted setting.
    ConfigOption {
        id: "shell_tool".to_string(),
        name: "Shell tool".to_string(),
        description: Some(
            "Which shell the agent runs commands through. Bash and PowerShell are separate \
             tools with their own syntax, permission rules, and prompt guidance."
                .to_string(),
        ),
        category: Some("agent".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "auto".to_string(),
        options: vec![
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
        ],
    }
}

fn claude_codex_fallback_config_option() -> ConfigOption {
    ConfigOption {
        id: "claude_codex_fallback".to_string(),
        name: "Claude/Codex fallback".to_string(),
        description: Some(
            "Load compatible skills and commands from .claude and .codex directories. \
             Takes effect after restarting Rebon."
                .to_string(),
        ),
        category: Some("agent".to_string()),
        option_type: ConfigOptionType::Select,
        current_value: "off".to_string(),
        options: vec![
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
        ],
    }
}
