//! Terminal-level tests for `rebon_session_runtime::ultraplan_review`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use crate::session::ultraplan_review::*;
use crate::tui::runner::test_support::make_test_tui_session;

#[derive(Default)]
struct AlwaysOkSubAgentSpawner {
    spawned: AtomicUsize,
}

#[async_trait::async_trait]
impl rebon_tool::SubAgentSpawner for AlwaysOkSubAgentSpawner {
    async fn spawn(
        &self,
        _spec: rebon_tool::SubAgentSpec,
    ) -> Result<rebon_tool::SubAgentResult, String> {
        Err("synchronous spawn is not used by this test".into())
    }

    async fn spawn_background(&self, _spec: rebon_tool::SubAgentSpec) -> Result<String, String> {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        Ok("reviewer-ok".into())
    }
}

#[derive(Default)]
struct FailOnceSubAgentSpawner {
    calls: AtomicUsize,
    specs: Mutex<Vec<rebon_tool::SubAgentSpec>>,
    retried: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl rebon_tool::SubAgentSpawner for FailOnceSubAgentSpawner {
    async fn spawn(
        &self,
        _spec: rebon_tool::SubAgentSpec,
    ) -> Result<rebon_tool::SubAgentResult, String> {
        Err("synchronous spawn is not used by this test".into())
    }

    async fn spawn_background(&self, spec: rebon_tool::SubAgentSpec) -> Result<String, String> {
        self.specs.lock().unwrap().push(spec);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            Err("transient reviewer startup failure".into())
        } else {
            self.retried.notify_one();
            Ok("reviewer-retry".into())
        }
    }
}

#[test]
fn review_prompt_includes_hash_and_step_ids() {
    let prompt = build_review_prompt("task", "### Step 1: One\nP2. Two", "abc", 7);
    assert!(prompt.contains("Expected plan_hash: abc"));
    assert!(prompt.contains("Expected base_revision: 7"));
    assert!(prompt.contains("- P1"));
    assert!(prompt.contains("- P2"));
    assert!(prompt.contains("StructuredOutput"));
    assert!(prompt.contains("blocker, advisory, or out_of_scope"));
}

#[tokio::test]
async fn start_review_retries_once_after_a_stale_reservation_cas() {
    let mut session = crate::tui::runner::test_support::make_ultraplan_test_tui_session();
    let temp = tempfile::TempDir::new().expect("temp dir");
    session.projects_root = temp.path().to_path_buf();
    session.cwd = "review-cas-retry".into();
    let spawner = Arc::new(AlwaysOkSubAgentSpawner::default());
    session.engine_half.sub_agent_spawner = spawner.clone();
    let workspace = tempfile::tempdir().expect("workspace dir");
    let workspace_root = workspace.path().to_string_lossy().to_string();

    let plan = "P1. Ship safely\n- files: src.rs\n- change: update\n- verify: cargo test\n";
    let mut state = rebon_types::UltraplanRunState::new(
        "run-cas-retry".into(),
        session.session_id.clone(),
        "task".into(),
        None,
        1,
    );
    state.record_interview_turn("Scope?".into(), None, "Narrow".into());
    state.set_plan_artifacts(
        plan.into(),
        rebon_types::PlanCoverageResult {
            covered: Vec::new(),
            missing: Vec::new(),
            unknown_ids: Vec::new(),
        },
        Vec::new(),
    );
    let mut capability = rebon_types::CapabilityContext {
        run_id: state.run_id.clone(),
        ledger_revision: state.ledger_revision,
        requirements_hash: state.requirements_hash.clone(),
        session_id: session.session_id.clone(),
        cwd: workspace_root.clone(),
        allowed_roots: vec![workspace_root],
        read_allowed: true,
        write_allowed: false,
        shell_allowed: false,
        tool_ids: vec![
            "Read".into(),
            "Glob".into(),
            "Grep".into(),
            "StructuredOutput".into(),
        ],
        network: rebon_types::NetworkCapability::Denied,
        workspace_head: None,
        workspace_dirty: None,
        provider: None,
        model: None,
        sub_agent_available: true,
        max_research_agents: state.budget.max_research_agents,
        research_agents_used: state.budget.research_agents_used,
        max_adversarial_reviews: state.budget.max_adversarial_reviews,
        adversarial_reviews_used: state.budget.adversarial_reviews_used,
        max_tool_error_retries: state.budget.max_tool_error_retries,
        capability_hash: String::new(),
    };
    capability.refresh_hash();
    state.set_capability_context(capability);
    let plan_hash = state.plan_hash.clone().expect("plan hash");
    rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();

    // The gate's in-memory snapshot goes stale: a concurrent writer bumps
    // the persisted state_revision without touching plan, stage, or budget.
    let stale =
        rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, &state.run_id)
            .unwrap();
    let mut concurrent = stale.clone();
    concurrent.record_tool_error_attempts("test:concurrent-bump".into(), 1);
    rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &concurrent).unwrap();

    start_review(&session, &stale, plan, &plan_hash, ReviewKind::Auto)
        .expect("stale reservation should reload and retry once");

    let persisted =
        rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, &state.run_id)
            .unwrap();
    assert_eq!(persisted.budget.adversarial_reviews_used, 1);
    assert_eq!(
        persisted.pending_review_plan_hash.as_deref(),
        Some(plan_hash.as_str())
    );
}

#[tokio::test]
async fn reviewer_startup_failure_retries_once_with_lineage_and_same_budget_slot() {
    let mut session = make_test_tui_session();
    let temp = tempfile::TempDir::new().expect("temp dir");
    session.projects_root = temp.path().to_path_buf();
    session.cwd = "review-startup-retry".into();
    let spawner = Arc::new(FailOnceSubAgentSpawner::default());
    session.engine_half.sub_agent_spawner = spawner.clone();

    let mut state = rebon_types::UltraplanRunState::new(
        "run-startup-retry".into(),
        session.session_id.clone(),
        "task".into(),
        None,
        1,
    );
    state.budget.adversarial_reviews_used = 1;
    state.prepare_for_persist();
    rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();

    let mut spec = rebon_tool::SubAgentSpec::new("review");
    spec.metadata = json!({"agent_id": "reviewer-original"});
    spawn_review_background(&session, &state, "plan-hash", spec, 1, true).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        spawner.retried.notified(),
    )
    .await
    .expect("reviewer retry should start");

    assert_eq!(spawner.calls.load(Ordering::SeqCst), 2);
    let specs = spawner.specs.lock().unwrap();
    assert_eq!(specs.len(), 2);
    assert_eq!(
        specs[1].metadata["retry_of"].as_str(),
        Some("reviewer-original")
    );
    assert_eq!(
        specs[1].metadata["ultraplan_retry_attempt"].as_bool(),
        Some(true)
    );
    assert_eq!(specs[1].metadata["retry_attempt"].as_u64(), Some(1));
    drop(specs);

    let persisted = rebon_session::load_ultraplan_run(
        &session.projects_root,
        &session.cwd,
        "run-startup-retry",
    )
    .unwrap();
    assert_eq!(persisted.budget.adversarial_reviews_used, 1);
}
