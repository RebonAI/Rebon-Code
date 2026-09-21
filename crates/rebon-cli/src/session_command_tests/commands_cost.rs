//! Terminal-level tests for `rebon_session_runtime::commands::cost`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use crate::rebon_config::{resolve_model_pricing, ModelPricingCatalog, ModelTokenPricing};
use crate::session::commands::cost::*;
use crate::session::commands::ConfigDirGuard;
use rebon_types::Usage;

use crate::tui::app::AppState;
use crate::tui::runner::test_support::make_test_tui_session;
use tempfile::TempDir;

#[test]
fn historical_cost_buckets_model_switches_and_excludes_exact_unknown_models() {
    let app = AppState::new();
    let priced_usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        cache_read_input_tokens: 1_000_000,
        cache_creation_input_tokens: 1_000_000,
        ..Default::default()
    };
    let unpriced_usage = Usage {
        input_tokens: 9_000_000,
        output_tokens: 9_000_000,
        cache_read_input_tokens: 9_000_000,
        cache_creation_input_tokens: 9_000_000,
        ..Default::default()
    };
    app.usage_mut().add_turn("provider", "known", priced_usage);
    app.usage_mut()
        .add_turn("provider", "known-next", unpriced_usage);
    app.usage_mut().add_turn(
        "provider",
        "known",
        Usage {
            input_tokens: 1,
            ..Default::default()
        },
    );

    assert_eq!(app.usage().by_model.len(), 2);
    assert_eq!(
        app.usage()
            .by_model
            .get(&("provider".into(), "known".into()))
            .unwrap()
            .input_tokens,
        1_000_001
    );
    assert_eq!(app.usage().total.input_tokens, 10_000_001);

    let mut catalog = ModelPricingCatalog::new();
    catalog.entry("provider".into()).or_default().insert(
        "known".into(),
        ModelTokenPricing {
            input: 1.0,
            output: 2.0,
            cache_read: 3.0,
            cache_write: 4.0,
        },
    );
    let rendered = format_historical_usd_cost(
        &app.usage().by_model,
        ("ignored", "fallback", app.usage().total),
        Some(&catalog),
    );

    assert!(rendered.contains("provider/known\n"), "{rendered}");
    assert!(rendered.contains("input: $1.000001"), "{rendered}");
    assert!(rendered.contains("output: $2.000000"), "{rendered}");
    assert!(rendered.contains("cache read: $3.000000"), "{rendered}");
    assert!(rendered.contains("cache write: $4.000000"), "{rendered}");
    assert!(
        rendered.contains("provider/known-next: unpriced (excluded from total)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("total: $10.000001 (unpriced usage excluded)"),
        "{rendered}"
    );
    assert!(!rendered.contains("$100."), "{rendered}");
}

#[test]
fn historical_cost_uses_config_catalog_override_rates() {
    let usage = Usage {
        input_tokens: 500_000,
        ..Default::default()
    };
    // The model table prices this model on its own; the override is what
    // beats it. $20/M over half a million tokens is $10.
    let table_rate = resolve_model_pricing(&Default::default(), "anthropic", "claude-sonnet-4-5")
        .expect("the model table prices Sonnet");
    assert_ne!(
        table_rate.input, 20.0,
        "the override has to differ to prove anything"
    );

    let mut catalog = ModelPricingCatalog::new();
    catalog.entry("anthropic".into()).or_default().insert(
        "claude-sonnet-4-5".into(),
        ModelTokenPricing {
            input: 20.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
    );
    let rendered = format_historical_usd_cost(
        &std::collections::BTreeMap::new(),
        ("anthropic", "claude-sonnet-4-5", usage),
        Some(&catalog),
    );
    assert!(rendered.contains("input: $10.000000"), "{rendered}");
    assert!(rendered.ends_with("total: $10.000000"), "{rendered}");
}

/// Without an override the table prices the session, which is the whole
/// point: `/cost` used to answer "no rates configured" for every model
/// anyone runs.
#[test]
fn historical_cost_prices_a_model_the_config_never_mentions() {
    let usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        ..Default::default()
    };
    let rendered = format_historical_usd_cost(
        &std::collections::BTreeMap::new(),
        ("openai", "gpt-6-astra", usage),
        Some(&ModelPricingCatalog::new()),
    );
    assert!(!rendered.contains("unpriced"), "{rendered}");
    // models.dev lists Astra at $10/M input and $50/M output.
    assert!(rendered.contains("input: $10.000000"), "{rendered}");
    assert!(rendered.contains("output: $50.000000"), "{rendered}");
    assert!(rendered.ends_with("total: $60.000000"), "{rendered}");
}

#[test]
fn historical_cost_does_not_double_charge_openai_shaped_cache_tokens() {
    let usage = Usage {
        input_tokens: 1_000_000,
        prompt_cache_hit_tokens: 600_000,
        prompt_cache_miss_tokens: 400_000,
        ..Default::default()
    };
    let mut catalog = ModelPricingCatalog::new();
    catalog.entry("deepseek".into()).or_default().insert(
        "deepseek-chat".into(),
        ModelTokenPricing {
            input: 10.0,
            output: 0.0,
            cache_read: 1.0,
            cache_write: 100.0,
        },
    );
    let rendered = format_historical_usd_cost(
        &std::collections::BTreeMap::new(),
        ("deepseek", "deepseek-chat", usage),
        Some(&catalog),
    );

    assert!(
        rendered.contains("input: $4.000000 (400000 @"),
        "{rendered}"
    );
    assert!(
        rendered.contains("cache read: $0.600000 (600000 @"),
        "{rendered}"
    );
    assert!(
        rendered.contains("cache write: $0.000000 (0 @"),
        "{rendered}"
    );
    assert!(rendered.ends_with("total: $4.600000"), "{rendered}");
}

#[test]
fn historical_cost_keeps_anthropic_shaped_cache_tokens_additive() {
    let usage = Usage {
        input_tokens: 1_000_000,
        cache_read_input_tokens: 1_000_000,
        cache_creation_input_tokens: 1_000_000,
        ..Default::default()
    };
    let mut catalog = ModelPricingCatalog::new();
    catalog.entry("anthropic".into()).or_default().insert(
        "exact-model".into(),
        ModelTokenPricing {
            input: 3.0,
            output: 0.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
    );
    let rendered = format_historical_usd_cost(
        &std::collections::BTreeMap::new(),
        ("anthropic", "exact-model", usage),
        Some(&catalog),
    );

    assert!(
        rendered.contains("input: $3.000000 (1000000 @"),
        "{rendered}"
    );
    assert!(
        rendered.contains("cache read: $0.300000 (1000000 @"),
        "{rendered}"
    );
    assert!(
        rendered.contains("cache write: $3.750000 (1000000 @"),
        "{rendered}"
    );
    assert!(rendered.ends_with("total: $7.050000"), "{rendered}");
}

#[test]
fn cost_summary_falls_back_to_defaults_and_reports_invalid_pricing_config() {
    let tempdir = TempDir::new().expect("tempdir");
    std::fs::write(
        tempdir.path().join("config.json"),
        r#"{"modelPricing":{"anthropic":{"claude-sonnet-4-5":{"input":1}}}}"#,
    )
    .expect("write config");
    let _config_dir = ConfigDirGuard::set(tempdir.path());

    let app = AppState::new();
    app.usage_mut().add_turn(
        "anthropic",
        "claude-sonnet-4-5",
        Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        },
    );
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let summary = historical_usd_cost_summary(&inputs, &session, app.usage().total);

    assert!(
        summary.contains("model pricing config invalid:"),
        "{summary}"
    );
    assert!(summary.contains("using built-in defaults"), "{summary}");
    assert!(summary.contains("input: $3.000000"), "{summary}");
    assert!(!summary.contains("no rates configured"), "{summary}");
}

/// On a ChatGPT subscription the dollars are a reference, not a bill: the
/// thing that runs out is plan credits. They are shown only for that
/// backend, and only because the rate card is the API price scaled by 25.
#[test]
fn cost_summary_reports_plan_credits_on_the_codex_backend() {
    let tempdir = TempDir::new().expect("tempdir");
    // `make_test_tui_session` runs on a provider called `test`, so that is
    // the entry the summary has to find and recognise.
    std::fs::write(
        tempdir.path().join("config.json"),
        r#"{"customProviders":[{"name":"test","format":"openai-responses","baseUrl":"https://chatgpt.com/backend-api/codex/responses","apiKey":"$OPENAI_OAUTH_TOKEN","model":"gpt-6-astra"}]}"#,
    )
    .expect("write config");
    let _config_dir = ConfigDirGuard::set(tempdir.path());

    let app = AppState::new();
    app.usage_mut().add_turn(
        "test",
        "gpt-6-astra",
        Usage {
            // $10/M input and $50/M output = $60, and 25 credits to the
            // dollar makes 1,500.
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        },
    );
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let summary = historical_usd_cost_summary(&inputs, &session, app.usage().total);

    assert!(summary.contains("total: $60.000000"), "{summary}");
    assert!(summary.contains("ChatGPT plan credits"), "{summary}");
    assert!(summary.contains("1500 credits"), "{summary}");
}

/// A provider billed in dollars does not get a credits line — there are no
/// credits to report, and inventing them would misstate the bill.
#[test]
fn cost_summary_reports_no_credits_off_the_codex_backend() {
    let tempdir = TempDir::new().expect("tempdir");
    std::fs::write(
        tempdir.path().join("config.json"),
        r#"{"customProviders":[{"name":"test","format":"openai-responses","baseUrl":"https://api.openai.com/v1","apiKey":"sk-x","model":"gpt-6-astra"}]}"#,
    )
    .expect("write config");
    let _config_dir = ConfigDirGuard::set(tempdir.path());

    let app = AppState::new();
    app.usage_mut().add_turn(
        "test",
        "gpt-6-astra",
        Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        },
    );
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let summary = historical_usd_cost_summary(&inputs, &session, app.usage().total);

    assert!(summary.contains("total: $10.000000"), "{summary}");
    assert!(!summary.contains("credits"), "{summary}");
}

#[test]
fn cost_summary_hints_pricing_keys_when_every_model_is_unpriced() {
    let tempdir = TempDir::new().expect("tempdir");
    let _config_dir = ConfigDirGuard::set(tempdir.path());

    let app = AppState::new();
    app.usage_mut().add_turn(
        "deepseek",
        "deepseek-chat",
        Usage {
            input_tokens: 10,
            ..Default::default()
        },
    );
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let summary = historical_usd_cost_summary(&inputs, &session, app.usage().total);

    assert!(
        summary.contains("no rates configured for these models: deepseek/deepseek-chat"),
        "{summary}"
    );
    assert!(summary.contains("modelPricing"), "{summary}");
    assert!(summary.contains("cacheWrite"), "{summary}");
    assert!(
        !summary.contains("model pricing config invalid:"),
        "{summary}"
    );
}

/// The defect this ledger exists to end: a hosted session's turns run in
/// a worker, which answers `/cost` from an `AppState` it builds for the
/// occasion. While the count lived on that state, the answer was the
/// zeros of a state no turn had ever passed through.
///
/// Four steps, each the production one: run a turn into the session's
/// ledger the way `finish_background_turn` does, build the command state
/// the way the worker's IPC server does, read the inputs off it, and ask
/// `/cost`.
#[test]
fn a_turn_the_worker_ran_is_in_the_cost_it_reports() {
    let session = make_test_tui_session();
    session
        .engine_half
        .usage_ledger
        .lock()
        .expect("usage ledger poisoned")
        .add_turn(
            "anthropic",
            "claude-sonnet-4-5",
            Usage {
                input_tokens: 23_000,
                output_tokens: 1_000,
                ..Default::default()
            },
        );

    let worker_inputs = crate::background::WorkerCommandInputs::from_session(
        &session,
        crate::ui_config::UiMode::default(),
    );
    let inputs = worker_inputs.inputs(&session);
    let rendered = execute_cost_command(&session, &inputs);

    assert!(rendered.contains("total input: 23.0k"), "{rendered}");
    assert!(rendered.contains("total output: 1.0k"), "{rendered}");
    assert!(rendered.contains("last turn input: 23.0k"), "{rendered}");
    let usage = &inputs.usage;
    assert_eq!(usage.total.input_tokens, 23_000);
    assert_eq!(usage.last_turn.output_tokens, 1_000);
    assert_eq!(
        usage
            .by_model
            .get(&("anthropic".to_string(), "claude-sonnet-4-5".to_string()))
            .map(|bucket| bucket.input_tokens),
        Some(23_000),
        "the per-model bucket is what prices the turn"
    );
}
