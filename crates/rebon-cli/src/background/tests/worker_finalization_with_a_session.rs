//! The one finalization test that needs a built session.
//!
//! `publish_turn_usage` takes a session, and the only fixture that builds one
//! is the terminal's `make_test_tui_session`. The rest of the finalization
//! tests live with the worker in `rebon-session-runtime`; this one stays
//! here because its fixture cannot leave the binary.

use std::path::PathBuf;

use crate::session::host::{BackgroundRuntimeFields, BackgroundStore};

fn store() -> (tempfile::TempDir, BackgroundStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BackgroundStore::new(dir.path());
    (dir, store)
}

fn runtime() -> BackgroundRuntimeFields {
    BackgroundRuntimeFields {
        provider: None,
        model: None,
        fast_mode: None,
        channels: Vec::new(),
        development_channels: Vec::new(),
        provider_format: None,
        ui_mode: None,
        effort_level: None,
        permission_mode: None,
        capability_mode: rebon_types::AgentCapabilityMode::Normal,
        settings: Vec::new(),
        add_dirs: Vec::new(),
        plugin_dirs: Vec::new(),
        mcp_configs: Vec::new(),
        strict_mcp_config: false,
    }
}

#[test]
fn a_finished_turn_lands_in_the_ledger_and_in_the_job_record() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let session = crate::tui::runner::test_support::make_test_tui_session();

    crate::session::host::worker::execution::publish_turn_usage(
        &session,
        &store,
        &job.identity.job_id,
        &rebon_types::Usage {
            input_tokens: 23_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 500,
            ..Default::default()
        },
    );

    let ledger = session
        .engine_half
        .usage_ledger
        .lock()
        .expect("usage ledger poisoned")
        .snapshot();
    assert_eq!(ledger.total.input_tokens, 23_000);
    assert_eq!(ledger.last_turn.output_tokens, 1_000);
    assert_eq!(ledger.by_model.len(), 1, "the turn is priced by its model");

    let published = store
        .read_state(&job.identity.job_id)
        .unwrap()
        .outcome
        .usage
        .expect("the job record carries the mirror's snapshot");
    assert_eq!(published.input_tokens, 23_000);
    assert_eq!(published.output_tokens, 1_000);
    assert_eq!(published.cache_read_tokens, 500);
}
