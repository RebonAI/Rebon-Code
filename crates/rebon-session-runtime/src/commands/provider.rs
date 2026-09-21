//! `/provider` — the configured providers, and the runtime built from them.
//!
//! Everything here is a function over `config.json`: list what is
//! configured, add a provider from a preset or by hand, add a model to one,
//! pin a role to a model in a profile, remove one, switch to one. None of it
//! needs a terminal, and the background worker reaches the same
//! configuration, so the command belongs beside the session rather than
//! inside the view that happens to type it.
//!
//! [`execute_provider_reconnect`] is the exception that proves the split:
//! every other entry point is pure over configuration, and that one has to
//! reach the live session's runtime cache.

use rebon_slash_commands::strip_command_prefix;

use crate::commands::{command_args, name_ends_here};
use crate::EngineSession;

#[derive(Debug, Clone)]
pub struct ProviderRuntimeUpdate {
    pub provider_name: String,
    pub model_name: String,
}

pub struct ProviderCommandResult {
    pub text: String,
    pub is_err: bool,
    pub runtime_update: Option<ProviderRuntimeUpdate>,
}

/// Returns `Some(())` when the text names `/provider`.
pub fn parse_provider_command(text: &str) -> Option<()> {
    name_ends_here(strip_command_prefix(text, "provider")?).then_some(())
}

pub fn handle_provider_command(text: &str) -> ProviderCommandResult {
    let args = command_args(text, "provider");
    let parts: Vec<&str> = if args.is_empty() {
        vec![]
    } else {
        args.split_whitespace().collect()
    };
    let subcommand = parts.first().map(|s| s.to_lowercase());
    let subcommand = subcommand.as_deref().unwrap_or("list");

    match subcommand {
        "list" | "ls" => provider_list(),
        "add" => provider_add(&parts[1..]),
        "add-model" | "addmodel" => provider_add_model(&parts[1..]),
        "models" | "discover" | "fetch-models" => provider_models(&parts[1..]),
        "profile" | "profiles" => provider_profile(&parts[1..]),
        "remove" | "rm" => provider_remove(&parts[1..]),
        "use" => provider_use(&parts[1..]),
        _ if parts.len() == 1 => provider_use(&parts),
        _ => ProviderCommandResult {
            text: provider_usage_text(),
            is_err: false,
            runtime_update: None,
        },
    }
}

/// `/provider reconnect` — drop the cached provider runtimes.
///
/// Separate from [`handle_provider_command`] because that one is a pure
/// function over configuration, and this one has to reach the session's cache.
pub fn execute_provider_reconnect(session: &EngineSession) -> String {
    match session.reconnect_provider_runtimes() {
        Ok(0) => "No provider runtime was connected; the next turn starts one.".to_string(),
        Ok(1) => "1 provider runtime dropped. The next turn reconnects.".to_string(),
        Ok(n) => format!("{n} provider runtimes dropped. The next turn reconnects."),
        Err(reason) => format!("!{reason}"),
    }
}

fn provider_quick_setup_line(preset: &crate::rebon_config::ProviderPreset) -> String {
    format!("  {} ({})", preset.display_name, preset.id)
}

fn provider_quick_setup_lines() -> Vec<String> {
    crate::rebon_config::provider_presets()
        .iter()
        .map(provider_quick_setup_line)
        .collect()
}

fn provider_usage_text() -> String {
    let mut lines: Vec<String> = [
        "Usage:",
        "  /provider                 Open provider switcher",
        "  /provider add             Open protected provider setup",
        "  /provider list            List configured providers",
        "  /provider add-model <name> <model>",
        "  /provider models <name>   List the endpoint's models into the provider",
        "  /provider profile <name>  Show or edit the model-profile table",
        "  /provider remove <name>",
        "  /provider reconnect       Drop cached provider runtimes and redial",
        "  /provider use <name>",
        "  /provider use default",
        "",
        "Provider setup:",
        "  Enter API keys in the protected form. Keys are masked and are not stored in prompt history.",
        "",
        "Available presets:",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    lines.extend(provider_quick_setup_lines());
    lines.extend(
        [
            "",
            "API protocols available in the form:",
            "  openai            OpenAI Chat Completions protocol",
            "  openai-responses  OpenAI Responses API protocol",
            "  anthropic         Anthropic Messages protocol",
            "",
            "API keys may reference $VAR or ${VAR}; references are resolved at runtime.",
        ]
        .into_iter()
        .map(str::to_string),
    );
    lines.join("\n")
}

fn provider_list() -> ProviderCommandResult {
    use rebon_provider::provider_catalog::{
        discover_plugin_model_providers, provider_catalog, ProviderOrigin,
    };

    let config_home = crate::rebon_config::config_home_dir();
    // The same discovery the settings window uses, so the two surfaces cannot
    // disagree about which providers exist. Mid-session: a vanished cwd only
    // hides project-scoped plugin providers, so it is not an error here.
    let cwd = std::env::current_dir().unwrap_or_default();
    let plugins = discover_plugin_model_providers(&config_home, &cwd);
    let catalog = provider_catalog(&config_home, &plugins);

    if catalog.is_empty() {
        return ProviderCommandResult {
            text: "No custom providers configured.\n\nRun /provider add to open the protected setup form. API keys entered there are masked and are not stored in prompt history."
                .into(),
            is_err: false,
            runtime_update: None,
        };
    }

    let any_active = catalog.iter().any(|entry| entry.is_active);
    let mut lines = vec!["Custom Providers:".to_string(), String::new()];
    for entry in &catalog {
        let marker = if entry.is_active { " (active)" } else { "" };
        lines.push(format!("  {}{}", entry.display_name, marker));
        // Which half of the pairing supplied this row. A user provider whose
        // requests actually go through a plugin's process is the case worth
        // naming: nothing else on screen would tell you.
        match &entry.origin {
            ProviderOrigin::User => {}
            ProviderOrigin::UserWithPlugin { plugin } => {
                lines.push(format!(
                    "    Source: config + plugin `{plugin}` (transport)"
                ));
            }
            ProviderOrigin::Plugin { plugin } => {
                lines.push(format!("    Source: plugin `{plugin}` only"));
            }
        }
        if let Some(format) = &entry.format {
            lines.push(format!("    Format: {format}"));
        }
        if let Some(base_url) = &entry.base_url {
            lines.push(format!("    URL:    {base_url}"));
        }
        if let Some(key) = &entry.api_key_masked {
            lines.push(format!("    Key:    {key}"));
        }
        if let Some(model) = &entry.default_model {
            lines.push(format!("    Model:  {model}"));
        }
        // Show the full models list when more than just the active
        // model is known. Old configs only have `model`; in that
        // case we leave this row off so output stays compact.
        let only_default = entry.models.len() == 1
            && entry.default_model.as_deref() == Some(entry.models[0].as_str());
        if entry.models.len() > 1 || (entry.models.len() == 1 && !only_default) {
            lines.push(format!("    Models: [{}]", entry.models.join(", ")));
        }
        // Declared profile roles, so a surprising model choice for titles or
        // sub-agents is visible here rather than only in a bill.
        let profiles = entry
            .model_profiles
            .iter()
            .map(|(role, model)| format!("{role}={model}"))
            .collect::<Vec<_>>();
        if !profiles.is_empty() {
            lines.push(format!("    Profiles: {}", profiles.join(", ")));
        }
        if let Some(reason) = &entry.unusable_reason {
            lines.push(format!("    Unusable: {reason}"));
        }
        lines.push(String::new());
    }

    if !any_active {
        lines.push("Currently using: no provider active".into());
    }

    ProviderCommandResult {
        text: lines.join("\n"),
        is_err: false,
        runtime_update: None,
    }
}

fn provider_add(parts: &[&str]) -> ProviderCommandResult {
    use crate::rebon_config::{
        add_custom_provider, add_custom_provider_from_preset, mask_api_key, provider_preset_by_id,
        VALID_PROVIDER_FORMATS,
    };

    if parts.len() == 1 || parts.len() == 2 {
        if let Some(preset) = parts.first().and_then(|name| provider_preset_by_id(name)) {
            // A preset whose endpoint takes no key (Ollama) is complete
            // with the bare command; everything else goes to the masked
            // form rather than taking a key from the prompt history.
            if parts.len() == 1 && preset.api_key_required {
                return ProviderCommandResult {
                    text: provider_preset_usage_text(preset),
                    is_err: true,
                    runtime_update: None,
                };
            }
            let api_key = parts.get(1).copied().unwrap_or("");
            let info = match add_custom_provider_from_preset(preset.id, api_key) {
                Ok(info) => info,
                Err(err) => {
                    return ProviderCommandResult {
                        text: err.to_string(),
                        is_err: true,
                        runtime_update: None,
                    };
                }
            };
            let model_line = if info.model.is_empty() {
                format!(
                    "(none yet — run /provider models {} to list the endpoint's models)",
                    info.name
                )
            } else {
                info.model.clone()
            };
            let key_line = if preset.api_key_required {
                mask_api_key(api_key)
            } else {
                "(not needed)".to_string()
            };
            let mut text = format!(
                "{} provider added and activated.\n\n\
                 \x20 Format: {}\n\
                 \x20 URL:    {}\n\
                 \x20 Key:    {}\n\
                 \x20 Model:  {}",
                preset.display_name, preset.format, preset.base_url, key_line, model_line
            );
            let placeholders = preset.base_url_placeholders();
            if !placeholders.is_empty() {
                text.push_str(&format!(
                    "\n\n  Replace {} in the URL before use (edit the provider with /provider add).",
                    placeholders
                        .iter()
                        .map(|t| format!("{{{t}}}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            return ProviderCommandResult {
                text,
                is_err: false,
                runtime_update: Some(runtime_update_from_info(&info)),
            };
        }
    }

    if parts.len() < 5 {
        return ProviderCommandResult {
            text: "Missing arguments. Run /provider add to enter credentials in the protected setup form. Inline credential syntax remains accepted for compatibility but is intentionally not displayed."
                .into(),
            is_err: true,
            runtime_update: None,
        };
    }

    let name = parts[0];
    let raw_format = parts[1];
    let base_url = parts[2];
    let api_key = parts[3];
    let model: String = parts[4..].join(" ");
    let format = raw_format.to_lowercase();

    if !VALID_PROVIDER_FORMATS.contains(&format.as_str()) {
        return ProviderCommandResult {
            text: format!(
                "Invalid format \"{raw_format}\". Must be: {}",
                VALID_PROVIDER_FORMATS.join(" | ")
            ),
            is_err: true,
            runtime_update: None,
        };
    }

    let info = match add_custom_provider(name, &format, base_url, api_key, &model) {
        Ok(info) => info,
        Err(err) => {
            return ProviderCommandResult {
                text: err.to_string(),
                is_err: true,
                runtime_update: None,
            };
        }
    };
    ProviderCommandResult {
        text: format!(
            "Provider \"{name}\" added and activated.\n\n\
             \x20 Format: {format}\n\
             \x20 URL:    {base_url}\n\
             \x20 Key:    {}\n\
             \x20 Model:  {model}",
            mask_api_key(api_key)
        ),
        is_err: false,
        runtime_update: Some(runtime_update_from_info(&info)),
    }
}

fn provider_preset_usage_text(preset: &crate::rebon_config::ProviderPreset) -> String {
    let mut lines = vec![
        format!(
            "{} preset is available in the protected provider form.",
            preset.display_name
        ),
        "Run /provider add, choose the preset, then enter the API key in the masked field."
            .to_string(),
        String::new(),
        "The preset configures:".to_string(),
        format!("  Format: {}", preset.format),
        format!("  URL:    {}", preset.base_url),
        format!("  Model:  {}", preset.default_model),
    ];
    let context_windows: Vec<String> = preset
        .models()
        .iter()
        .filter_map(|model| {
            model
                .context_window
                .map(|window| format!("    {}: {}", model.id, window))
        })
        .collect();
    if !context_windows.is_empty() {
        lines.push("  Context windows:".to_string());
        lines.extend(context_windows);
    }
    lines.join("\n")
}

fn provider_add_model(parts: &[&str]) -> ProviderCommandResult {
    use crate::rebon_config::add_custom_provider_model;

    if parts.len() < 2 {
        return ProviderCommandResult {
            text: "Usage: /provider add-model <name> <model>".into(),
            is_err: true,
            runtime_update: None,
        };
    }

    let name = parts[0];
    let model: String = parts[1..].join(" ");

    match add_custom_provider_model(name, &model) {
        Ok(info) => ProviderCommandResult {
            text: format!(
                "Model \"{model}\" added to \"{name}\" (now active).\n  Models: [{}]",
                info.models.join(", ")
            ),
            is_err: false,
            runtime_update: Some(runtime_update_from_info(&info)),
        },
        Err(err) => ProviderCommandResult {
            text: err.to_string(),
            is_err: true,
            runtime_update: None,
        },
    }
}

/// `/provider models <name>` — ask the provider's endpoint which models it
/// serves and fold them into the entry, the way the desktop settings
/// window's model-list refresh does.
///
/// Blocks for the request (a few seconds at most; the client has a 20s
/// timeout). A vendor with no list endpoint gets its documented catalogue,
/// and the reply says so.
fn provider_models(parts: &[&str]) -> ProviderCommandResult {
    use crate::rebon_config::{get_active_custom_provider_name, sync_custom_provider_models};

    let name = match parts.first() {
        Some(name) => (*name).to_string(),
        None => match get_active_custom_provider_name() {
            Some(active) => active,
            None => {
                return ProviderCommandResult {
                    text: "Usage: /provider models <name> (no provider is active)".into(),
                    is_err: true,
                    runtime_update: None,
                };
            }
        },
    };

    match sync_custom_provider_models(&name) {
        Ok(sync) => {
            let mut lines = vec![match &sync.fallback_reason {
                Some(reason) => format!(
                    "No model list endpoint for \"{}\" ({reason}); filled in the documented catalogue.",
                    sync.info.name
                ),
                None => format!(
                    "Listed {} models for \"{}\" from {}.",
                    sync.info.models.len(),
                    sync.info.name,
                    sync.source
                ),
            }];
            if !sync.added.is_empty() {
                lines.push(format!("  Added:   {}", sync.added.join(", ")));
            }
            if sync.windows_filled > 0 {
                lines.push(format!(
                    "  Limits filled in for {} model(s).",
                    sync.windows_filled
                ));
            }
            if sync.skipped_non_chat > 0 {
                lines.push(format!(
                    "  Skipped {} non-chat model(s).",
                    sync.skipped_non_chat
                ));
            }
            if sync.skipped_other_wire > 0 {
                lines.push(format!(
                    "  {} model(s) speak another wire format; switch the provider's format to reach them.",
                    sync.skipped_other_wire
                ));
            }
            lines.push(format!("  Model:   {}", sync.info.model));
            lines.push(format!("  Models:  [{}]", sync.info.models.join(", ")));
            ProviderCommandResult {
                text: lines.join("\n"),
                is_err: false,
                runtime_update: Some(runtime_update_from_info(&sync.info)),
            }
        }
        Err(err) => ProviderCommandResult {
            text: err.to_string(),
            is_err: true,
            runtime_update: None,
        },
    }
}

/// `/provider profile` — read and edit one provider's `modelProfiles`.
///
/// This is the TUI half of a surface the desktop settings window has had all
/// along. Without it the only way to fix a profile table from a terminal was
/// to hand-edit `config.json` — and a wrong table is not a cosmetic problem:
/// it decides which model pays for titles, auto-mode classifications,
/// background summaries and compaction.
fn provider_profile(parts: &[&str]) -> ProviderCommandResult {
    use crate::rebon_config::{custom_provider_profiles, set_custom_provider_profile};

    let Some(name) = parts.first() else {
        return ProviderCommandResult {
            text: provider_profile_usage_text(),
            is_err: true,
            runtime_update: None,
        };
    };

    // `/provider profile <name>` — just show the table.
    if parts.len() == 1 {
        return match custom_provider_profiles(name) {
            Ok(rows) => ProviderCommandResult {
                text: render_provider_profiles(name, &rows),
                is_err: false,
                runtime_update: None,
            },
            Err(err) => ProviderCommandResult {
                text: err.to_string(),
                is_err: true,
                runtime_update: None,
            },
        };
    }

    if parts.len() < 3 {
        return ProviderCommandResult {
            text: provider_profile_usage_text(),
            is_err: true,
            runtime_update: None,
        };
    }

    let role = parts[1];
    // `follow` / `default` / `-` all mean "clear this role", which is how a
    // role goes back to running whatever model the session is running.
    let clears = matches!(
        parts[2].to_lowercase().as_str(),
        "follow" | "inherit" | "default" | "-" | "clear"
    );
    let (model, effort) = if clears {
        (None, None)
    } else {
        (Some(parts[2]), parts.get(3).copied())
    };

    match set_custom_provider_profile(name, role, model, effort) {
        Ok(rows) => {
            let headline = match model {
                Some(model) => match effort {
                    Some(effort) => format!("`{role}` on \"{name}\" → {model} (effort {effort})"),
                    None => format!("`{role}` on \"{name}\" → {model}"),
                },
                None => format!("`{role}` on \"{name}\" now follows the session's model"),
            };
            ProviderCommandResult {
                text: format!("{headline}\n\n{}", render_provider_profiles(name, &rows)),
                is_err: false,
                // Profiles are read per request, not baked into the runtime,
                // so nothing has to be rebuilt for this to take effect.
                runtime_update: None,
            }
        }
        Err(err) => ProviderCommandResult {
            text: err.to_string(),
            is_err: true,
            runtime_update: None,
        },
    }
}

fn render_provider_profiles(
    name: &str,
    rows: &[crate::rebon_config::ProviderProfileEntry],
) -> String {
    let mut lines = vec![format!("Model profiles for \"{name}\":"), String::new()];
    for row in rows {
        let value = match (&row.model, &row.reasoning_effort) {
            (Some(model), Some(effort)) => format!("{model}  (effort {effort})"),
            (Some(model), None) => model.clone(),
            (None, _) => "follows the session's model".to_string(),
        };
        lines.push(format!("  {:<10} {}", row.role, value));
    }
    lines.push(String::new());
    lines.push(
        "An undeclared role runs whatever model the session is on — it does not fall back"
            .to_string(),
    );
    lines.push("to `general` or to the provider's default model.".to_string());
    lines.join("\n")
}

fn provider_profile_usage_text() -> String {
    [
        "Usage:",
        "  /provider profile <name>                        Show the profile table",
        "  /provider profile <name> <role> <model>         Pin a role to a model",
        "  /provider profile <name> <role> <model> <effort>  … with a reasoning effort",
        "  /provider profile <name> <role> follow          Clear it (follow the session)",
        "",
        "Roles: general, small, fast, explore, librarian, builder, reviewer, reasoning",
        "Efforts: low, medium, high, xhigh, max",
        "",
        "`small` is the one that pays for titles, auto-mode classification, background",
        "summaries and compaction. Everything undeclared follows the session's model.",
    ]
    .join("\n")
}

fn provider_remove(parts: &[&str]) -> ProviderCommandResult {
    use crate::rebon_config::remove_custom_provider;

    let Some(name) = parts.first() else {
        return ProviderCommandResult {
            text: "Usage: /provider remove <name>".into(),
            is_err: true,
            runtime_update: None,
        };
    };

    match remove_custom_provider(name) {
        Ok(was_active) => ProviderCommandResult {
            text: format!(
                "Provider \"{name}\" removed.{}",
                if was_active {
                    " Provider deactivated."
                } else {
                    ""
                }
            ),
            is_err: false,
            runtime_update: was_active.then_some(ProviderRuntimeUpdate {
                provider_name: "env".into(),
                model_name: rebon_harness::default_model(),
            }),
        },
        Err(err) => ProviderCommandResult {
            text: err.to_string(),
            is_err: true,
            runtime_update: None,
        },
    }
}

fn provider_use(parts: &[&str]) -> ProviderCommandResult {
    use crate::rebon_config::{
        clear_active_custom_provider, mask_api_key, resolve_env_value, set_active_custom_provider,
    };

    let Some(name) = parts.first() else {
        return ProviderCommandResult {
            text: "Usage: /provider use <name>  or  /provider use default".into(),
            is_err: true,
            runtime_update: None,
        };
    };

    if name.to_lowercase() == "default" {
        if let Err(err) = clear_active_custom_provider() {
            return ProviderCommandResult {
                text: format!("Failed to deactivate provider: {err}"),
                is_err: true,
                runtime_update: None,
            };
        }
        return ProviderCommandResult {
            text: "Provider deactivated.".into(),
            is_err: false,
            runtime_update: Some(ProviderRuntimeUpdate {
                provider_name: "env".into(),
                model_name: rebon_harness::default_model(),
            }),
        };
    }

    match set_active_custom_provider(name) {
        Ok(info) => {
            let resolved_key = resolve_env_value(&info.api_key);
            ProviderCommandResult {
                text: format!(
                    "Switched to \"{}\" ({}).\n  URL:   {}\n  Key:   {}\n  Model: {}",
                    info.name,
                    info.format,
                    resolve_env_value(&info.base_url),
                    mask_api_key(&resolved_key),
                    resolve_env_value(&info.model),
                ),
                is_err: false,
                runtime_update: Some(runtime_update_from_info(&info)),
            }
        }
        Err(err) => ProviderCommandResult {
            text: err.to_string(),
            is_err: true,
            runtime_update: None,
        },
    }
}

pub(crate) fn runtime_update_from_info(
    info: &crate::rebon_config::CustomProviderInfo,
) -> ProviderRuntimeUpdate {
    ProviderRuntimeUpdate {
        provider_name: info.name.clone(),
        model_name: crate::rebon_config::resolve_env_value(&info.model),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_preset_without_key_routes_to_protected_form() {
        let result = handle_provider_command("/provider add deepseek");
        assert!(result.is_err);
        assert!(result
            .text
            .contains("DeepSeek preset is available in the protected provider form"));
        assert!(result.text.contains("Run /provider add"));
        assert!(!result.text.contains("<apiKey>"));
        assert!(result.text.contains("https://api.deepseek.com"));
        assert!(result.text.contains("deepseek-v4-flash: 1000000"));
        assert!(result.text.contains("deepseek-v4-pro: 1000000"));
        assert!(result.runtime_update.is_none());
    }

    fn with_temp_config_dir<T>(f: impl FnOnce() -> T) -> T {
        let _env_lock = crate::test_env::lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", tmp.path());
        let result = f();
        restore_env_var("REBON_CONFIG_DIR", previous);
        result
    }

    fn restore_env_var(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn provider_quick_add_deepseek_persists_and_activates() {
        let result = with_temp_config_dir(|| {
            handle_provider_command("/provider add deepseek $DEEPSEEK_API_KEY")
        });

        assert!(!result.is_err, "{}", result.text);
        assert!(result
            .text
            .contains("DeepSeek provider added and activated"));
        assert!(result.text.contains("Key:    $DEEPSEEK_API_KEY"));
        let update = result.runtime_update.expect("runtime update");
        assert_eq!(update.provider_name, "deepseek");
        assert_eq!(update.model_name, "deepseek-v4-flash");
    }

    #[test]
    fn provider_quick_add_other_presets_persists_and_activates() {
        for (command, provider_name, model_name) in [
            ("/provider add glm $ZHIPUAI_API_KEY", "glm", "glm-5.3"),
            ("/provider add kimi $MOONSHOT_API_KEY", "kimi", "kimi-k3"),
            (
                "/provider add minimax $MINIMAX_API_KEY",
                "minimax",
                "MiniMax-M3",
            ),
            (
                "/provider add qwen $DASHSCOPE_API_KEY",
                "qwen",
                "qwen3.8-max",
            ),
            (
                "/provider add openai $OPENAI_API_KEY",
                "openai",
                "gpt-5.6-sol",
            ),
            ("/provider add ollama", "ollama", ""),
        ] {
            let result = with_temp_config_dir(|| handle_provider_command(command));

            assert!(!result.is_err, "{}", result.text);
            let update = result.runtime_update.expect("runtime update");
            assert_eq!(update.provider_name, provider_name);
            assert_eq!(update.model_name, model_name);
        }
    }

    #[test]
    fn provider_manual_add_still_works() {
        let result = with_temp_config_dir(|| {
            handle_provider_command(
                "/provider add local openai http://localhost:11434/v1 ollama llama3",
            )
        });

        assert!(!result.is_err, "{}", result.text);
        assert!(result
            .text
            .contains("Provider \"local\" added and activated"));
        let update = result.runtime_update.expect("runtime update");
        assert_eq!(update.provider_name, "local");
        assert_eq!(update.model_name, "llama3");
    }

    /// How an undeclared role renders in `/provider profile <name>`.
    const SMALL_FOLLOWS_SESSION: &str = "small      follows the session's model";

    #[test]
    fn provider_profile_pins_a_role_and_clears_it_back_to_following_the_session() {
        let (pinned, cleared, listed) = with_temp_config_dir(|| {
            handle_provider_command(
                "/provider add local openai http://localhost:11434/v1 ollama llama3",
            );
            handle_provider_command("/provider add-model local llama3-mini");
            let pinned = handle_provider_command("/provider profile local small llama3-mini");
            let cleared = handle_provider_command("/provider profile local small follow");
            let listed = handle_provider_command("/provider profile local");
            (pinned, cleared, listed)
        });

        assert!(!pinned.is_err, "{}", pinned.text);
        assert!(pinned.text.contains("`small` on \"local\" → llama3-mini"));
        assert!(pinned.text.contains("small      llama3-mini"));

        assert!(!cleared.is_err, "{}", cleared.text);
        assert!(cleared.text.contains("follows the session's model"));

        // Every known role is listed, and the still-unset ones say so rather
        // than reporting some inherited value.
        assert!(!listed.is_err, "{}", listed.text);
        for role in crate::rebon_config::PROVIDER_PROFILE_ROLES {
            let listed_role = listed.text.contains(*role);
            assert!(listed_role, "role {role} missing:\n{}", listed.text);
        }
        let small_follows = listed.text.contains(SMALL_FOLLOWS_SESSION);
        assert!(small_follows, "{}", listed.text);
    }

    #[test]
    fn provider_profile_reports_bad_input_without_writing() {
        let (bad_role, missing_provider, listed) = with_temp_config_dir(|| {
            handle_provider_command(
                "/provider add local openai http://localhost:11434/v1 ollama llama3",
            );
            let bad_role = handle_provider_command("/provider profile local smal llama3");
            let missing_provider = handle_provider_command("/provider profile ghost small llama3");
            let listed = handle_provider_command("/provider profile local");
            (bad_role, missing_provider, listed)
        });

        assert!(bad_role.is_err, "{}", bad_role.text);
        assert!(bad_role.text.contains("Unknown profile role"));
        assert!(missing_provider.is_err, "{}", missing_provider.text);
        let small_untouched = listed.text.contains(SMALL_FOLLOWS_SESSION);
        assert!(small_untouched, "{}", listed.text);
    }

    #[test]
    fn provider_usage_advertises_the_profile_subcommand() {
        let usage = provider_usage_text();
        assert!(usage.contains("/provider profile <name>"));
    }
}
