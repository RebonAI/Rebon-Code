//! The `ProfileSwitch` / `ProfileSave` gate — one decision, every surface.
//!
//! [`rebon_plugin_profile::proposal`] decides what the prompt says and what
//! approving it does. What lives here is the part that sits between that
//! decision and a permission query: resolving the proposal before the prompt
//! is raised, refusing one outright, and carrying an approved one out.
//!
//! It is here rather than in the terminal because a background worker takes
//! the same two decisions. A worker has no screen, but it does have the
//! session — its prompts are answered by whoever opens the job — and until
//! this moved, a worker fell through to a generic prompt showing the model's
//! own words instead of the computed diff, and an approval there applied
//! nothing at all: `ProfileSwitch::call` refuses to claim the session moved
//! without the report only an applying surface writes.
//!
//! Each surface still supplies its own [`ProfileSession`] — which handle is
//! which is genuinely the front end's — and its own way of parking the
//! provider/model re-resolve. The decision is not.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use rebon_acp::DefaultHandler;
use rebon_acp_client::AcpAgentBackend;
use rebon_agent_core::routing::SessionAgents;
use rebon_core::permission::{OutboundPermissionQuery, PermissionAnswer};
use rebon_harness::build_default_tool_filter_for_context;
use rebon_permissions::PermissionMode;
use rebon_plugin_profile::{
    applied_input, apply_approved_proposal as apply_proposal_to_session, resolve_proposal_action,
    save_proposal, switch_proposal, ProfileProposal, ProfileProposalAction, ProfileSession,
    RuntimeRefresh,
};
use rebon_tool::{SharedCoordinatorMode, SharedToolFilter, ToolFilter};

use crate::permission_answer::{
    build_permission_answer_with_extra_text, build_permission_answer_with_input,
};
use crate::EngineSession;

/// What the gate decided about an outbound permission query.
pub enum ProfileProposalOutcome {
    /// Raise the prompt. Either the call was not a profile proposal at all, or
    /// it was one and the resolved proposal now rides on the query's metadata.
    Prompt(OutboundPermissionQuery),
    /// Refused before anyone was asked. The query has been answered on its own
    /// channel and the model already has the reason; the surface only has to
    /// drop whatever view it had started building.
    Refused,
}

/// The two halves of carrying out an approved proposal.
///
/// `result` is what the tool is handed back — the only thing that makes
/// `ProfileSwitch::call` report success. `runtime_refresh` is the
/// provider/model move that still has to reach the model client, which needs
/// `&mut` on the session and a tokio handle that no permission callback holds.
/// It is returned rather than performed so each surface can park it where it
/// has both.
pub struct AppliedProposal {
    pub result: Result<Value, String>,
    pub runtime_refresh: Option<RuntimeRefresh>,
}

/// Intercept a profile proposal on its way to the prompt.
///
/// Returns [`ProfileProposalOutcome::Refused`] when the call was turned away
/// outright — a profile nobody saved, or one carrying `bypassPermissions`.
/// Otherwise the query comes back with the resolved proposal attached to its
/// metadata, which is what a surface turns into a profile-shaped dialog.
///
/// Runs here rather than where the dialog is built because that one has no
/// session to compare against, and the whole point of the diff is that it is
/// computed from the live session rather than quoted from the model.
pub fn resolve_profile_proposal(
    target: &mut dyn ProfileSession,
    config_dir: &Path,
    mut outbound: OutboundPermissionQuery,
) -> ProfileProposalOutcome {
    let Some(action) = resolve_proposal_action(&outbound.tool_name) else {
        return ProfileProposalOutcome::Prompt(outbound);
    };
    let input = outbound.tool_input.clone().unwrap_or(Value::Null);

    let built = match action {
        ProfileProposalAction::Switch => switch_proposal(target, config_dir, &input),
        ProfileProposalAction::Save => save_proposal(config_dir, &input),
    };
    match built {
        Ok(proposal) => {
            outbound.metadata = Some(proposal.to_permission_metadata());
            ProfileProposalOutcome::Prompt(outbound)
        }
        Err(reason) => {
            // Same shape the ultraplan gate uses: answer the query as a
            // rejection and hand the reason back as the model's tool feedback.
            let answer =
                build_permission_answer_with_extra_text(Some("reject_once".into()), Some(reason));
            let _ = outbound.response_tx.send(answer);
            ProfileProposalOutcome::Refused
        }
    }
}

/// Carry out an approved proposal.
///
/// `target` is `None` on a surface that approved the call without a session to
/// move; the proposal then reaches disk and nothing else, exactly as
/// [`rebon_plugin_profile::apply_approved_proposal`] defines it.
pub fn apply_approved_proposal(
    target: Option<&mut dyn ProfileSession>,
    config_dir: &Path,
    proposal: &ProfileProposal,
) -> AppliedProposal {
    let outcome = apply_proposal_to_session(target, config_dir, proposal);
    // Read out before the verdict: a partial apply may already have moved the
    // provider on disk, and a session left on the old client would fail every
    // request after a failure it was told was only partial.
    AppliedProposal {
        result: outcome.result,
        runtime_refresh: outcome.runtime_refresh,
    }
}

/// The answer to send once a proposal has been applied.
pub fn approved_answer(proposal: &ProfileProposal, result: Value) -> PermissionAnswer {
    build_permission_answer_with_input(
        Some("allow_once".into()),
        Some(applied_input(proposal, result)),
    )
}

/// The answer to send when the user approved but applying failed.
///
/// Reporting the approval alone would leave the model believing the session
/// had moved when it had not, so the approval is downgraded to a rejection
/// carrying the reason.
pub fn not_applied_answer(reason: &str) -> PermissionAnswer {
    build_permission_answer_with_extra_text(
        Some("reject_once".into()),
        Some(format!(
            "The user approved this, but it could not be applied: {reason}"
        )),
    )
}

/// This session seen through the handles a profile needs, with no screen
/// behind it.
///
/// The terminal builds its own target off `AppState`, because the mode
/// indicator it draws lives there and has to move with the session. A worker
/// draws nothing, so its target is the session's own cells — and because it
/// owns clones rather than borrows, it can outlive the call that built it,
/// which is what lets [`spawn_profile_proposal_relay`] hold one across an
/// `await`.
pub(crate) struct DetachedProfileSession {
    session_id: String,
    model_name: String,
    queue_session: bool,
    handler: DefaultHandler,
    session_agents: Arc<SessionAgents<AcpAgentBackend>>,
    tool_filter: SharedToolFilter,
    coordinator_mode: SharedCoordinatorMode,
    permission_mode_cell: Arc<Mutex<PermissionMode>>,
}

impl DetachedProfileSession {
    pub fn from_session(session: &EngineSession) -> Self {
        Self {
            session_id: session.session_id.clone(),
            model_name: session.model.name.clone(),
            queue_session: session.startup.queue_session,
            handler: session.engine_half.handler.clone(),
            session_agents: Arc::clone(&session.engine_half.runtime.session_agents),
            tool_filter: session.engine_half.session_filter_handle.clone(),
            coordinator_mode: session.engine_half.coordinator_mode_handle.clone(),
            permission_mode_cell: Arc::clone(&session.engine_half.permission_mode_cell),
        }
    }
}

impl ProfileSession for DetachedProfileSession {
    fn model_name(&self) -> String {
        self.model_name.clone()
    }

    fn agent_id(&self) -> String {
        self.session_agents.current_id()
    }

    fn known_agent_ids(&self) -> Vec<String> {
        self.session_agents
            .choices()
            .into_iter()
            .map(|choice| choice.id)
            .collect()
    }

    fn switch_agent(&self, id: &str) -> Result<String, String> {
        self.session_agents.switch_to(id)
    }

    fn permission_mode(&self) -> PermissionMode {
        *self
            .permission_mode_cell
            .lock()
            .expect("mode cell poisoned")
    }

    /// Written through the cell the engine's broker reads and pushed into the
    /// session record, which is the pair the terminal's own adapter writes.
    ///
    /// What is deliberately *not* written is the global "background jobs may
    /// run in this mode" acceptance the terminal records on a Shift+Tab. That
    /// one is a standing decision about every future job, and a worker is the
    /// last place it should be granted from — this approval was about this
    /// session.
    fn set_permission_mode(&mut self, mode: PermissionMode) {
        *self
            .permission_mode_cell
            .lock()
            .expect("mode cell poisoned") = mode;
        let _ =
            self.handler
                .apply_config_option_local(&self.session_id, "permissions", mode.as_wire());
    }

    fn tool_filter(&self) -> ToolFilter {
        self.tool_filter.current()
    }

    fn set_tool_filter(&self, filter: ToolFilter) {
        self.tool_filter.set(filter);
    }

    fn default_tool_filter(&self) -> ToolFilter {
        build_default_tool_filter_for_context(self.coordinator_mode.get(), self.queue_session)
    }
}

/// Put the profile gate in front of a permission channel that has no screen.
///
/// A worker's prompts are answered over IPC by whoever opens the job, and the
/// answer goes straight back to the tool — so there was no point on that road
/// holding the session, and a profile approval there applied nothing. The
/// relay is that point: it resolves the proposal before the prompt is
/// published (so the person sees the computed diff rather than the model's
/// own words), then stands in for the tool's own reply channel so an approval
/// comes back through here, where the session is.
///
/// Everything that is not a resolved proposal is forwarded untouched, and
/// **every** way of not being approved — a refusal, a cancellation, a prompt
/// dropped without an answer, an option this surface cannot carry out — leaves
/// the session exactly as it was. The returned receiver replaces the one the
/// caller passed in.
pub(crate) fn spawn_profile_proposal_relay(
    session: &EngineSession,
    inbound: UnboundedReceiver<OutboundPermissionQuery>,
) -> UnboundedReceiver<OutboundPermissionQuery> {
    relay_proposals_through(
        DetachedProfileSession::from_session(session),
        crate::rebon_config::config_home_dir(),
        inbound,
    )
}

/// [`spawn_profile_proposal_relay`] over any session and any profile store —
/// which is what lets the relay's fail-closed behaviour be pinned without
/// booting a worker or touching the user's own `~/.rebon`.
fn relay_proposals_through<S: ProfileSession + Send + 'static>(
    mut target: S,
    config_dir: std::path::PathBuf,
    mut inbound: UnboundedReceiver<OutboundPermissionQuery>,
) -> UnboundedReceiver<OutboundPermissionQuery> {
    let (forward_tx, forward_rx) = unbounded_channel();

    tokio::spawn(async move {
        while let Some(outbound) = inbound.recv().await {
            let mut outbound = match resolve_profile_proposal(&mut target, &config_dir, outbound) {
                // Already answered on its own channel; the model has the reason.
                ProfileProposalOutcome::Refused => continue,
                ProfileProposalOutcome::Prompt(outbound) => outbound,
            };
            let Some(proposal) = outbound
                .metadata
                .as_ref()
                .and_then(ProfileProposal::from_permission_metadata)
            else {
                if forward_tx.send(outbound).is_err() {
                    return;
                }
                continue;
            };

            // Hand the surface a stand-in so the answer passes back through
            // here. The tool's own sender is a `oneshot` that can only be sent
            // once, so holding it is also what guarantees exactly one reply.
            let (stand_in_tx, stand_in_rx) = tokio::sync::oneshot::channel();
            let tool_tx = std::mem::replace(&mut outbound.response_tx, stand_in_tx);
            let label = proposal.label.clone();
            if forward_tx.send(outbound).is_err() {
                let _ = tool_tx.send(PermissionAnswer::Cancelled);
                return;
            }

            let answer = match stand_in_rx.await {
                Ok(answer) => answer,
                // Nobody answered and the prompt is gone — the job was
                // cancelled, the turn ended, the endpoint dropped it. An
                // unanswered proposal is an unapproved one.
                Err(_) => PermissionAnswer::Cancelled,
            };
            let answer = match answer {
                PermissionAnswer::Selected { ref option_id, .. } if option_id == "allow_once" => {
                    let applied =
                        apply_approved_proposal(Some(&mut target), &config_dir, &proposal);
                    // Not parked for a re-resolve the way the terminal parks
                    // it: a provider move is written to `config.json`, and a
                    // worker builds a fresh session — resolving the runtime
                    // from that same file — for every turn it takes. The move
                    // lands on the next one without anyone holding it.
                    if let Some(refresh) = applied.runtime_refresh {
                        tracing::info!(
                            provider = %refresh.provider_name,
                            model = %refresh.model_name,
                            "rebon: an approved profile moved the provider; the worker's next turn resolves it"
                        );
                    }
                    match applied.result {
                        Ok(result) => approved_answer(&proposal, result),
                        Err(reason) => {
                            tracing::warn!(
                                profile = %label,
                                error = %reason,
                                "rebon: an approved profile could not be applied"
                            );
                            not_applied_answer(&reason)
                        }
                    }
                }
                // Anything else — a rejection, a cancellation, an
                // allow-always variant this surface cannot carry out — leaves
                // the session alone and reaches the tool unchanged, where the
                // missing report is what tells the model nothing moved.
                other => other,
            };
            let _ = tool_tx.send(answer);
        }
    });

    forward_rx
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    /// A session with nothing behind it. The gate's own decisions do not
    /// depend on which handles a surface has, which is the point of taking
    /// the trait rather than a concrete session.
    #[derive(Default)]
    struct FakeSession {
        /// Shared so a test can still read the mode after the relay has taken
        /// ownership of the session.
        observed_mode: Arc<Mutex<Option<PermissionMode>>>,
        filter: Mutex<Option<ToolFilter>>,
    }

    impl ProfileSession for FakeSession {
        fn model_name(&self) -> String {
            "vendor-pro".into()
        }

        fn agent_id(&self) -> String {
            "local".into()
        }

        fn known_agent_ids(&self) -> Vec<String> {
            vec!["local".into()]
        }

        fn switch_agent(&self, id: &str) -> Result<String, String> {
            Ok(id.to_string())
        }

        fn permission_mode(&self) -> PermissionMode {
            self.observed_mode
                .lock()
                .unwrap()
                .unwrap_or(PermissionMode::Default)
        }

        fn set_permission_mode(&mut self, mode: PermissionMode) {
            *self.observed_mode.lock().unwrap() = Some(mode);
        }

        fn tool_filter(&self) -> ToolFilter {
            self.filter
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(ToolFilter::unrestricted)
        }

        fn set_tool_filter(&self, filter: ToolFilter) {
            *self.filter.lock().unwrap() = Some(filter);
        }

        fn default_tool_filter(&self) -> ToolFilter {
            ToolFilter::unrestricted()
        }
    }

    fn query(tool_name: &str, input: Value) -> (OutboundPermissionQuery, ResponseRx) {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        (
            OutboundPermissionQuery {
                id: 1,
                tool_name: tool_name.into(),
                tool_call_id: "call-1".into(),
                session_id: "sess-1".into(),
                title: "title".into(),
                message: "message".into(),
                tool_input: Some(input),
                metadata: None,
                options: Vec::new(),
                response_tx,
            },
            response_rx,
        )
    }

    type ResponseRx = tokio::sync::oneshot::Receiver<PermissionAnswer>;

    /// A call that is not a profile proposal passes through untouched — the
    /// gate must not put metadata on someone else's query.
    #[test]
    fn a_call_that_is_not_a_proposal_reaches_the_prompt_unchanged() {
        let tmp = TempDir::new().unwrap();
        let mut session = FakeSession::default();
        let (outbound, _rx) = query("Bash", json!({"command": "ls"}));

        let outcome = resolve_profile_proposal(&mut session, tmp.path(), outbound);

        let ProfileProposalOutcome::Prompt(outbound) = outcome else {
            panic!("an unrelated tool is not the gate's business");
        };
        assert_eq!(outbound.metadata, None);
    }

    /// A profile nobody saved is refused before the user is asked, and the
    /// model is told why on the query's own channel.
    #[test]
    fn a_profile_that_does_not_exist_is_refused_without_a_prompt() {
        let tmp = TempDir::new().unwrap();
        let mut session = FakeSession::default();
        let (outbound, mut rx) = query(
            rebon_plugin_profile::PROFILE_SWITCH_TOOL_NAME,
            json!({"profile": "nobody-saved-this", "reason": "prose"}),
        );

        let outcome = resolve_profile_proposal(&mut session, tmp.path(), outbound);

        assert!(
            matches!(outcome, ProfileProposalOutcome::Refused),
            "an unknown profile never reaches the user"
        );
        let PermissionAnswer::Selected { option_id, .. } =
            rx.try_recv().expect("the query was answered")
        else {
            panic!("a refusal is a selection, not a cancellation");
        };
        assert_eq!(option_id, "reject_once");
    }

    /// An approval that could not be applied is handed back as a rejection,
    /// so the model does not report a session move that never happened.
    #[test]
    fn a_failed_apply_answers_as_a_rejection_carrying_the_reason() {
        let answer = not_applied_answer("the provider is not configured");

        let PermissionAnswer::Selected {
            option_id,
            extra_text,
            ..
        } = answer
        else {
            panic!("a downgraded approval is a selection");
        };
        assert_eq!(option_id, "reject_once");
        assert!(
            extra_text
                .as_deref()
                .is_some_and(|text| text.contains("the provider is not configured")),
            "{extra_text:?}"
        );
    }

    /// The report has to reach the tool as its own input; without that key
    /// `ProfileSwitch::call` refuses to claim the session moved.
    #[test]
    fn an_approved_proposal_answers_with_the_report_on_the_input() {
        let proposal = ProfileProposal {
            action: ProfileProposalAction::Switch,
            profile_id: "writing".into(),
            label: "Writing".into(),
            reason: None,
            rows: Vec::new(),
            notes: Vec::new(),
            original_input: json!({"profile": "writing", "reason": "prose"}),
        };

        let answer = approved_answer(&proposal, json!({"profile": "writing"}));

        let PermissionAnswer::Selected { updated_input, .. } = answer else {
            panic!("an approved proposal is a selection");
        };
        let updated = updated_input.expect("carries the report");
        assert!(updated
            .get(rebon_plugin_profile::PROFILE_SWITCH_APPLIED_KEY)
            .is_some());
        assert_eq!(updated["profile"], json!("writing"));
    }

    // ── the relay a surface with no screen runs ─────────────────────────

    /// A saved profile that only narrows the permission mode, so applying it
    /// needs nothing off disk beyond the profile itself.
    fn save_edits_profile(config_dir: &Path) {
        rebon_config::profile_store::save(
            config_dir,
            &rebon_config::profile_store::Profile {
                permission_mode: Some("acceptEdits".into()),
                ..rebon_config::profile_store::Profile::new("edits")
            },
        )
        .expect("the profile is saved");
    }

    fn switch_query(profile: &str) -> (OutboundPermissionQuery, ResponseRx) {
        query(
            rebon_plugin_profile::PROFILE_SWITCH_TOOL_NAME,
            json!({"profile": profile, "reason": "the next few turns are prose"}),
        )
    }

    /// A sender whose receiver is dropped straight away, for swapping a
    /// query's reply channel out in a test.
    fn oneshot_sink() -> tokio::sync::oneshot::Sender<PermissionAnswer> {
        tokio::sync::oneshot::channel().0
    }

    /// The red half, kept next to the green one: this is what a worker did
    /// before the relay existed. Its answer went from the IPC endpoint
    /// straight back to the tool, so an approval arrived carrying no report —
    /// and the tool refuses to claim the session moved on the strength of an
    /// approval alone.
    #[tokio::test]
    async fn an_approval_that_passed_no_applying_surface_reports_failure() {
        use rebon_tool::Tool;

        let error = rebon_plugin_profile::ProfileSwitchTool
            .call(
                json!({"profile": "edits", "reason": "the next few turns are prose"}),
                &rebon_tool::ToolContext::default(),
            )
            .await
            .expect_err("a bare approval must not read as a switch");

        assert!(
            error.to_string().contains("/profile use"),
            "the model is pointed at the road that does work: {error}"
        );
    }

    /// The whole point of the relay: what reaches the person answering is the
    /// diff computed from this session, not the sentence the model wrote.
    #[tokio::test]
    async fn a_forwarded_proposal_carries_the_resolved_diff() {
        let tmp = TempDir::new().unwrap();
        save_edits_profile(tmp.path());
        let (inbound_tx, inbound_rx) = unbounded_channel();
        let mut forwarded =
            relay_proposals_through(FakeSession::default(), tmp.path().into(), inbound_rx);

        let (outbound, _tool_rx) = switch_query("edits");
        inbound_tx.send(outbound).unwrap();

        let published = forwarded.recv().await.expect("the prompt is forwarded");
        let proposal = published
            .metadata
            .as_ref()
            .and_then(ProfileProposal::from_permission_metadata)
            .expect("the resolved proposal rides on the metadata");
        assert_eq!(proposal.profile_id, "edits");
        assert!(
            !proposal.rows.is_empty(),
            "the diff names what would change rather than quoting the model"
        );
    }

    /// Fail-closed: a prompt nobody ever answers is not an approval. The
    /// session stays where it was and the tool is told the call was cancelled,
    /// which is what makes `ProfileSwitch::call` report failure rather than a
    /// move that never happened.
    #[tokio::test]
    async fn a_prompt_dropped_without_an_answer_leaves_the_session_alone() {
        let tmp = TempDir::new().unwrap();
        save_edits_profile(tmp.path());
        let (inbound_tx, inbound_rx) = unbounded_channel();
        let session = FakeSession::default();
        let observed_mode = Arc::clone(&session.observed_mode);
        let mut forwarded = relay_proposals_through(session, tmp.path().into(), inbound_rx);

        let (outbound, tool_rx) = switch_query("edits");
        inbound_tx.send(outbound).unwrap();
        let published = forwarded.recv().await.expect("the prompt is forwarded");

        // Nobody answers: the job was cancelled, the turn ended, the endpoint
        // went away. Dropping the query is how that reaches the relay.
        drop(published);

        assert!(
            matches!(
                tool_rx.await.expect("the tool is answered"),
                PermissionAnswer::Cancelled
            ),
            "an unanswered proposal must not become an approval"
        );
        assert_eq!(
            *observed_mode.lock().unwrap(),
            None,
            "nothing may be applied without an answer"
        );
    }

    /// A rejection reaches the tool untouched — nothing is applied, and the
    /// model is not handed a report that would let it claim otherwise.
    #[tokio::test]
    async fn a_rejected_proposal_applies_nothing() {
        let tmp = TempDir::new().unwrap();
        save_edits_profile(tmp.path());
        let (inbound_tx, inbound_rx) = unbounded_channel();
        let session = FakeSession::default();
        let observed_mode = Arc::clone(&session.observed_mode);
        let mut forwarded = relay_proposals_through(session, tmp.path().into(), inbound_rx);

        let (outbound, tool_rx) = switch_query("edits");
        inbound_tx.send(outbound).unwrap();
        let mut published = forwarded.recv().await.expect("the prompt is forwarded");

        let stand_in = std::mem::replace(&mut published.response_tx, oneshot_sink());
        stand_in
            .send(PermissionAnswer::Selected {
                option_id: "reject_once".into(),
                updated_input: None,
                extra_text: Some("not now".into()),
            })
            .expect("the relay is listening");

        let PermissionAnswer::Selected {
            option_id,
            updated_input,
            ..
        } = tool_rx.await.expect("the tool is answered")
        else {
            panic!("a rejection reaches the tool as a selection");
        };
        assert_eq!(option_id, "reject_once");
        assert!(
            updated_input.is_none(),
            "a rejection must not carry an applied report"
        );
        assert_eq!(*observed_mode.lock().unwrap(), None);
    }

    /// The green half: an approval passes back through the relay, which
    /// applies the profile to the session and hands the tool the report only
    /// an applying surface can write.
    #[tokio::test]
    async fn an_approval_applies_the_profile_and_reports_it() {
        let tmp = TempDir::new().unwrap();
        save_edits_profile(tmp.path());
        let (inbound_tx, inbound_rx) = unbounded_channel();
        let session = FakeSession::default();
        let observed_mode = Arc::clone(&session.observed_mode);
        let mut forwarded = relay_proposals_through(session, tmp.path().into(), inbound_rx);

        let (outbound, tool_rx) = switch_query("edits");
        inbound_tx.send(outbound).unwrap();
        let mut published = forwarded.recv().await.expect("the prompt is forwarded");

        let stand_in = std::mem::replace(&mut published.response_tx, oneshot_sink());
        stand_in
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .expect("the relay is listening");

        let PermissionAnswer::Selected { updated_input, .. } =
            tool_rx.await.expect("the tool is answered")
        else {
            panic!("an approval reaches the tool as a selection");
        };
        assert!(
            updated_input
                .as_ref()
                .and_then(|input| input.get(rebon_plugin_profile::PROFILE_SWITCH_APPLIED_KEY))
                .is_some(),
            "the report is what lets ProfileSwitch::call claim the session moved"
        );
        assert_eq!(
            *observed_mode.lock().unwrap(),
            Some(PermissionMode::AcceptEdits),
            "the session actually moved onto the profile's mode"
        );
    }

    /// A call that is not a proposal at all is forwarded untouched — the relay
    /// must not become a second gate on every other tool.
    #[tokio::test]
    async fn an_unrelated_call_passes_straight_through_the_relay() {
        let tmp = TempDir::new().unwrap();
        let (inbound_tx, inbound_rx) = unbounded_channel();
        let mut forwarded =
            relay_proposals_through(FakeSession::default(), tmp.path().into(), inbound_rx);

        let (outbound, tool_rx) = query("Bash", json!({"command": "ls"}));
        inbound_tx.send(outbound).unwrap();
        let mut published = forwarded.recv().await.expect("the call is forwarded");
        assert_eq!(published.tool_name, "Bash");
        assert_eq!(published.metadata, None);

        // Its own reply channel is still the tool's: the relay did not stand in
        // for a call it has no business applying.
        let tool_tx = std::mem::replace(&mut published.response_tx, oneshot_sink());
        tool_tx.send(PermissionAnswer::Cancelled).unwrap();
        assert!(matches!(
            tool_rx.await.expect("answered directly"),
            PermissionAnswer::Cancelled
        ));
    }
}
