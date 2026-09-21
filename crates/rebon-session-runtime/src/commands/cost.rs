//! `/cost`: what this session has spent, and what that costs in money.
//!
//! The token counters arrive through [`super::SessionCommandInputs`];
//! the pricing comes from the model catalog on disk. Neither is read
//! off a screen, which is why a worker answers this the same way.

use rebon_types::wall_clock_ms;

use super::{fmt_tokens, SessionCommandInputs};
use crate::EngineSession;

pub fn execute_cost_command(session: &EngineSession, inputs: &SessionCommandInputs) -> String {
    let usage = session.model.prune_level.budget.usage_snapshot();
    let window = session.model.prune_level.budget.context_window();
    let total = inputs.usage.total;
    let last = inputs.usage.last_turn;
    rebon_slash_commands::formatters::format_cost_command(
        rebon_slash_commands::formatters::CostCommandDto {
            duration: format_duration(wall_clock_ms().saturating_sub(inputs.usage.started_at_ms)),
            context_tokens: fmt_tokens(usage.tokens),
            context_window: fmt_tokens(window),
            context_source: usage.source.as_str().to_string(),
            streaming_tokens: fmt_tokens(inputs.streaming_token_count),
            total_input: fmt_tokens(total.input_tokens),
            total_output: fmt_tokens(total.output_tokens),
            total_cache_read: fmt_tokens(
                total
                    .cache_read_input_tokens
                    .saturating_add(total.prompt_cache_hit_tokens),
            ),
            total_cache_write: fmt_tokens(
                total
                    .cache_creation_input_tokens
                    .saturating_add(total.prompt_cache_miss_tokens),
            ),
            last_input: fmt_tokens(last.input_tokens),
            last_output: fmt_tokens(last.output_tokens),
            usd_summary: Some(historical_usd_cost_summary(inputs, session, total)),
        },
    )
}

/// Wrap the priced breakdown with the two conditions the formatter itself
/// cannot report: a rejected `modelPricing` override, and a session whose
/// every provider/model bucket is missing from the catalog.
pub fn historical_usd_cost_summary(
    inputs: &SessionCommandInputs,
    session: &EngineSession,
    total: rebon_types::Usage,
) -> String {
    let (pricing, load_error) =
        match crate::rebon_config::load_model_pricing(&crate::rebon_config::config_home_dir()) {
            Ok(catalog) => (catalog, None),
            Err(err) => (
                crate::rebon_config::default_model_pricing(),
                Some(err.to_string()),
            ),
        };
    let mut summary = format_historical_usd_cost(
        &inputs.usage.by_model,
        (&session.model.provider_name, &session.model.name, total),
        Some(&pricing),
    );
    if let Some(err) = load_error {
        summary.push_str(&format!(
            "\n  model pricing config invalid: {err}; using built-in defaults"
        ));
    }
    let observed = observed_cost_keys(
        &inputs.usage.by_model,
        (&session.model.provider_name, &session.model.name),
    );
    let all_unpriced = observed.iter().all(|(provider, model)| {
        crate::rebon_config::resolve_model_pricing(&pricing, provider, model).is_none()
    });
    if all_unpriced {
        let keys = observed
            .iter()
            .map(|(provider, model)| format!("{provider}/{model}"))
            .collect::<Vec<_>>()
            .join(", ");
        let config_key = crate::rebon_config::MODEL_PRICING_CONFIG_KEY;
        summary.push_str(&format!(
            "\n  no rates configured for these models: {keys}\n  set them in config.json under `{config_key}`: \"<provider>\": {{ \"<model>\": {{ \"input\": 0.0, \"output\": 0.0, \"cacheRead\": 0.0, \"cacheWrite\": 0.0 }} }}"
        ));
    }
    if let Some(credits) = codex_credit_summary(session, &inputs.usage.by_model, &pricing, total) {
        summary.push('\n');
        summary.push_str(&credits);
    }
    summary
}

/// ChatGPT credits per US dollar of the same model's API price.
///
/// The Codex rate card and the API price list are the same numbers scaled:
/// `gpt-6-astra` is $10/M input and 250 credits/M, `gpt-5.6-sol` $4 and 100,
/// `gpt-5.6-terra` $2 and 50, `gpt-5.6-luna` $0.20 and 5 — and the cached
/// and output rates hold the same ratio, twelve out of twelve. So the plan
/// currency is derived from the price the model table already carries
/// rather than kept as a second table that could go stale on its own.
const CODEX_CREDITS_PER_USD: f64 = 25.0;

/// What fast mode multiplies a request's credit cost by, per the Codex rate
/// card. The API's own docs say 2x for Astra and the Help Centre card says
/// 2.5x; the plan is billed by the card.
const CODEX_FAST_MULTIPLIER: f64 = 2.5;

/// The plan-currency view, for a session that is actually spending plan
/// credits rather than dollars.
///
/// Dollars are the wrong unit on a ChatGPT subscription: nothing is charged
/// in them, and the number that runs out is credits. Shown only for the
/// ChatGPT Codex backend — `api.openai.com` with a key really is billed in
/// dollars, and so is every other provider.
fn codex_credit_summary(
    session: &EngineSession,
    usage_by_model: &std::collections::BTreeMap<(String, String), rebon_types::Usage>,
    pricing: &crate::rebon_config::ModelPricingCatalog,
    fallback_total: rebon_types::Usage,
) -> Option<String> {
    if !session_runs_on_codex_backend(&session.model.provider_name) {
        return None;
    }
    let keys = observed_cost_keys(
        usage_by_model,
        (&session.model.provider_name, &session.model.name),
    );
    let mut credits = 0.0;
    let mut priced_any = false;
    for key in &keys {
        let (provider, model) = key;
        let usage = usage_by_model.get(key).unwrap_or(&fallback_total);
        let Some(price) = crate::rebon_config::resolve_model_pricing(pricing, provider, model)
        else {
            continue;
        };
        priced_any = true;
        credits += usd_for(usage, &price) * CODEX_CREDITS_PER_USD;
    }
    if !priced_any {
        return None;
    }

    let mut lines = vec![
        "ChatGPT plan credits".to_string(),
        format!("  total: {credits:.0} credits (1 credit = $0.04)"),
    ];
    // Fast mode is a per-request choice and a session can have been sent
    // both ways, so this is a ceiling rather than a bill: multiplying the
    // whole session by 2.5 would be a claim the transcript cannot support.
    if session.model.service_tier.is_fast() && session.model.service_tier_available {
        lines.push(format!(
            "  fast mode is on: turns sent with it bill {CODEX_FAST_MULTIPLIER}x, so up to {:.0} credits",
            credits * CODEX_FAST_MULTIPLIER
        ));
    }
    Some(lines.join("\n"))
}

/// Whether this session's provider is the ChatGPT Codex backend.
///
/// Read from the provider entry rather than from the session, which knows
/// the provider's name but not its endpoint.
fn session_runs_on_codex_backend(provider_name: &str) -> bool {
    crate::rebon_config::list_custom_providers()
        .into_iter()
        .find(|provider| provider.name.eq_ignore_ascii_case(provider_name))
        .is_some_and(|provider| rebon_api::is_chatgpt_codex_backend(&provider.base_url))
}

/// Provider/model buckets a cost report covers, falling back to the live
/// session pair before the first turn is recorded.
fn observed_cost_keys(
    usage_by_model: &std::collections::BTreeMap<(String, String), rebon_types::Usage>,
    fallback: (&str, &str),
) -> Vec<(String, String)> {
    if usage_by_model.is_empty() {
        vec![(fallback.0.to_string(), fallback.1.to_string())]
    } else {
        usage_by_model.keys().cloned().collect()
    }
}

/// The four token counts a bill is made of, after the per-vendor shapes are
/// reconciled.
struct BilledTokens {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

/// Split one usage record into what each rate applies to.
///
/// openai-shaped usage already counts hits and misses inside
/// `input_tokens`; only the hit slice moves to the cache-read rate, and
/// misses stay billed as plain input. Anthropic-shaped usage reports the
/// cache counts alongside `input_tokens` instead.
///
/// One function because two callers price the same session in two
/// currencies, and a second copy of this arithmetic would eventually
/// disagree with the first.
fn billed_tokens(usage: &rebon_types::Usage) -> BilledTokens {
    let openai_shaped = usage.input_tokens > 0
        && (usage.prompt_cache_hit_tokens > 0 || usage.prompt_cache_miss_tokens > 0);
    BilledTokens {
        input: u64::from(if openai_shaped {
            usage
                .input_tokens
                .saturating_sub(usage.prompt_cache_hit_tokens)
        } else {
            usage.input_tokens
        }),
        output: u64::from(usage.output_tokens),
        cache_read: u64::from(
            usage
                .cache_read_input_tokens
                .saturating_add(usage.prompt_cache_hit_tokens),
        ),
        cache_write: u64::from(if openai_shaped {
            usage.cache_creation_input_tokens
        } else {
            usage
                .cache_creation_input_tokens
                .saturating_add(usage.prompt_cache_miss_tokens)
        }),
    }
}

/// What one usage record costs at these rates, in USD.
fn usd_for(usage: &rebon_types::Usage, price: &crate::rebon_config::ModelTokenPricing) -> f64 {
    let billed = billed_tokens(usage);
    let cost = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    cost(billed.input, price.input)
        + cost(billed.output, price.output)
        + cost(billed.cache_read, price.cache_read)
        + cost(billed.cache_write, price.cache_write)
}

pub fn format_historical_usd_cost(
    usage_by_model: &std::collections::BTreeMap<(String, String), rebon_types::Usage>,
    fallback: (&str, &str, rebon_types::Usage),
    pricing: Option<&crate::rebon_config::ModelPricingCatalog>,
) -> String {
    let keys = observed_cost_keys(usage_by_model, (fallback.0, fallback.1));

    let mut lines = vec!["USD estimate".to_string()];
    let mut total_cost = 0.0;
    let mut has_unpriced = false;
    for key in &keys {
        let (provider, model) = key;
        let usage = usage_by_model.get(key).unwrap_or(&fallback.2);
        let rates = pricing.and_then(|catalog| {
            crate::rebon_config::resolve_model_pricing(catalog, provider, model)
        });
        let Some(price) = rates else {
            has_unpriced = true;
            lines.push(format!(
                "  {provider}/{model}: unpriced (excluded from total)"
            ));
            continue;
        };

        let BilledTokens {
            input: input_tokens,
            output: output_tokens,
            cache_read: cache_read_tokens,
            cache_write: cache_write_tokens,
        } = billed_tokens(usage);
        let cost = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
        let input = cost(input_tokens, price.input);
        let output = cost(output_tokens, price.output);
        let cache_read = cost(cache_read_tokens, price.cache_read);
        let cache_write = cost(cache_write_tokens, price.cache_write);
        let subtotal = input + output + cache_read + cache_write;
        total_cost += subtotal;
        lines.extend([
            format!("  {provider}/{model}"),
            format!(
                "    input: ${input:.6} ({input_tokens} @ ${:.4}/M)",
                price.input
            ),
            format!(
                "    output: ${output:.6} ({output_tokens} @ ${:.4}/M)",
                price.output
            ),
            format!(
                "    cache read: ${cache_read:.6} ({cache_read_tokens} @ ${:.4}/M)",
                price.cache_read
            ),
            format!(
                "    cache write: ${cache_write:.6} ({cache_write_tokens} @ ${:.4}/M)",
                price.cache_write
            ),
            format!("    subtotal: ${subtotal:.6}"),
        ]);
    }
    let exclusion = if has_unpriced {
        " (unpriced usage excluded)"
    } else {
        ""
    };
    lines.push(format!("  total: ${total_cost:.6}{exclusion}"));
    lines.join("\n")
}

fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}
