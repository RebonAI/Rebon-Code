//! `/model` -- switch the model the active provider runs.
//!
//! One function over `config.json`: read the configured providers, find the
//! active one, check the model against what that provider offers, and write
//! the choice back. What it returns alongside the text is the provider and
//! model pair the runtime should be rebuilt with, which is why it shares
//! [`ProviderRuntimeUpdate`] with `/provider` rather than owning a second
//! shape for the same fact.

use crate::commands::provider::{runtime_update_from_info, ProviderRuntimeUpdate};
use crate::commands::{command_args, name_ends_here};
use rebon_slash_commands::strip_command_prefix;

/// Download the model table, cache it, and install it for this process.
///
/// The download runs on a thread with a runtime of its own rather than on
/// the caller's: `/model` is handled from a synchronous command path that
/// may or may not be inside a tokio runtime, and `block_on` inside one
/// panics. The thread is joined, so `/model refresh` still reads as one
/// command that either worked or said why not.
fn refresh_model_table() -> (String, bool) {
    let url = rebon_api::model_table::table_url();
    let fetch_url = url.clone();
    let fetched = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => return Err(format!("could not start a runtime for the download: {err}")),
        };
        runtime.block_on(rebon_api::model_table::fetch_table(&fetch_url))
    })
    .join();

    let json = match fetched {
        Ok(Ok(json)) => json,
        Ok(Err(err)) => return (format!("Model table refresh failed: {err}"), true),
        Err(_) => {
            return (
                "Model table refresh failed: the download panicked".into(),
                true,
            )
        }
    };
    match crate::rebon_config::save_model_table(&json) {
        Ok(count) => {
            let (source, generated_at) = rebon_api::model_table::catalog_provenance();
            (
                format!(
                    "Model table refreshed from {url}: {count} models (source {source}, {generated_at}).\nCached at {}.",
                    crate::rebon_config::model_table_cache_path().display()
                ),
                false,
            )
        }
        Err(err) => (format!("Model table refresh failed: {err}"), true),
    }
}

pub struct ModelCommandResult {
    pub text: String,
    pub is_err: bool,
    pub runtime_update: Option<ProviderRuntimeUpdate>,
}

/// Returns `Some(())` when the text names `/model`.
pub fn parse_model_command(text: &str) -> Option<()> {
    name_ends_here(strip_command_prefix(text, "model")?).then_some(())
}

pub fn handle_model_command(text: &str) -> ModelCommandResult {
    use crate::rebon_config::{
        get_active_custom_provider_name, list_custom_providers, set_custom_provider_model,
    };

    let args = command_args(text, "model");
    let Some(active_provider) = get_active_custom_provider_name() else {
        return ModelCommandResult {
            text: "No custom provider is active. Activate one with /provider use <name> first."
                .into(),
            is_err: true,
            runtime_update: None,
        };
    };

    if args.is_empty() || args.eq_ignore_ascii_case("list") || args.eq_ignore_ascii_case("ls") {
        let provider = list_custom_providers()
            .into_iter()
            .find(|provider| provider.name.eq_ignore_ascii_case(&active_provider));
        let Some(provider) = provider else {
            return ModelCommandResult {
                text: format!("Active provider \"{active_provider}\" not found."),
                is_err: true,
                runtime_update: None,
            };
        };
        let choices = crate::rebon_config::provider_model_choices(&provider);
        let (configured, catalogue): (Vec<_>, Vec<_>) =
            choices.iter().partition(|choice| choice.configured);
        let ids = |choices: &[&crate::rebon_config::ModelChoice]| {
            choices
                .iter()
                .map(|choice| choice.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut text = format!(
            "Active provider: {}\nCurrent model: {}\nConfigured: [{}]",
            provider.name,
            provider.model,
            if configured.is_empty() {
                provider.model.clone()
            } else {
                ids(&configured)
            },
        );
        // The catalogue half is the answer to "what else can this key
        // reach": models nobody wrote into `models[]`, which used to be
        // invisible here and in the picker.
        if !catalogue.is_empty() {
            text.push_str(&format!(
                "\nAlso available ({}): [{}]",
                catalogue.len(),
                ids(&catalogue)
            ));
        }
        let (source, generated_at) = rebon_api::model_table::catalog_provenance();
        if !generated_at.is_empty() {
            text.push_str(&format!("\nModel table: {source} ({generated_at})"));
        }
        text.push_str("\n\nUse: /model <model>");
        return ModelCommandResult {
            text,
            is_err: false,
            runtime_update: None,
        };
    }

    if args.eq_ignore_ascii_case("refresh") {
        let (text, is_err) = refresh_model_table();
        return ModelCommandResult {
            text,
            is_err,
            runtime_update: None,
        };
    }

    if args.eq_ignore_ascii_case("default") {
        // "default" is not a model id: it clears the legacy global override
        // (which used to shadow every provider) and keeps the provider's own
        // configured model authoritative.
        let persist_warning = match crate::rebon_config::save_user_model(None) {
            Ok(()) => String::new(),
            Err(err) => format!("\nPersist warning: failed to write user settings: {err}"),
        };
        let current = list_custom_providers()
            .into_iter()
            .find(|provider| provider.name.eq_ignore_ascii_case(&active_provider))
            .map(|provider| provider.model)
            .unwrap_or_default();
        return ModelCommandResult {
            text: format!(
                "Cleared global model override. Provider \"{active_provider}\" uses \"{current}\".{persist_warning}"
            ),
            is_err: false,
            runtime_update: Some(ProviderRuntimeUpdate {
                provider_name: active_provider,
                model_name: crate::rebon_config::resolve_env_value(&current),
            }),
        };
    }

    match set_custom_provider_model(&active_provider, args) {
        Ok(info) => {
            // The model is persisted on the provider entry; clear any legacy
            // global override so it cannot shadow provider switches later.
            let persist_warning = match crate::rebon_config::save_user_model(None) {
                Ok(()) => String::new(),
                Err(err) => format!("\nPersist warning: failed to write user settings: {err}"),
            };
            ModelCommandResult {
                text: format!(
                    "Switched model for \"{}\" to \"{}\".{}",
                    info.name, info.model, persist_warning
                ),
                is_err: false,
                runtime_update: Some(runtime_update_from_info(&info)),
            }
        }
        Err(err) => ModelCommandResult {
            text: err.to_string(),
            is_err: true,
            runtime_update: None,
        },
    }
}
