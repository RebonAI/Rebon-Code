//! The listing that tells the model which skills it can invoke.
//!
//! One attachment, `skill_listing`: the names and one-line descriptions of the
//! loaded skills, as a **delta**. The first poll of a session lists everything;
//! every later poll lists only what registered since, which is how the
//! progressive-discovery subscriber's finds reach the model without
//! re-sending the catalogue each round. It is the read half of
//! [`Skill`](crate::SkillTool) — without the listing the model has names
//! it never saw.
//!
//! It lived in `rebon_core::attachments` and ran second of eight, right
//! behind `date_change`. It runs there still: this producer sits on the
//! kernel's `attachment-producers` seat at `Order::Listing`, and the date
//! roll took `Order::DayRoll` on the same seat when this one left, so the
//! two kept their relative order and both stayed ahead of the engine's own
//! session poller.
//!
//! **What it reads from outside the record.** One handle off the binding:
//! [`TurnSkillCatalog`] answers what this turn can invoke. The host binds
//! [`RegistrySkillCatalog`](crate::RegistrySkillCatalog), which reads the live
//! [`SkillRegistry`](crate::SkillRegistry) per poll, so a skill discovered in
//! this same turn is named on the next round.
//!
//! **What stayed behind.** `sent_skill_names` — the delta's memory — is
//! `rebon-session-state`'s, next to the plan-mode flags and the task
//! throttles. And the two header strings stayed in
//! `rebon_core::attachments`: the engine's run loop matches on them to drop
//! a listing that replayed history already carries verbatim, and it cannot
//! reach into a plugin to ask. This module renders from those same constants,
//! so the recognizer and the renderer cannot drift.

use std::collections::HashSet;
use std::sync::Arc;

#[cfg(test)]
use rebon_api::ContentBlock as ApiContentBlock;
use rebon_api::{make_meta_user_message, Message as ApiMessage};
use rebon_core::attachment_seat::{
    AvailableSkill, SeatAttachmentProducer, SessionAttachmentBinding, TurnSkillCatalog, TurnToolkit,
};
use rebon_core::attachments::{SKILL_LISTING_DELTA_HEADER, SKILL_LISTING_INITIAL_HEADER};
use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};
use rebon_session_state::{ServerState, SessionAttachmentState};

/// How many entries one listing names before it starts counting.
///
/// Deliberately low: the listing is history the model re-reads on every later
/// request, and a session with fifty skills would spend a paragraph on names
/// it will never invoke. The overflow line tells it the rest exist.
pub const SKILL_LISTING_NAME_CAP: usize = 15;

/// Immutable snapshot of everything the producer reads. Copied in by
/// [`SkillAttachmentPoller::poll`] so the producer doesn't hold a lock and so
/// tests can drive it with synthetic values.
#[derive(Debug, Clone)]
pub struct SkillListingInput {
    /// Snapshot of the session's attachment state. The producer only reads
    /// `sent_skill_names` from it; the addition goes back through the
    /// returned [`SkillListingStateDelta`].
    pub attachment_state: SessionAttachmentState,
    /// Skills currently available to the session (id + short description).
    pub available_skills: Vec<AvailableSkill>,
}

/// Side-effect bundle a poll wants applied to the session record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillListingStateDelta {
    /// When non-empty, append these skill names to `sent_skill_names`.
    pub add_sent_skill_names: Vec<String>,
}

/// Aggregate result of one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillListingOutput {
    /// Messages to push, in order.
    pub messages: Vec<ApiMessage>,
    /// State delta to apply to the session record.
    pub state_delta: SkillListingStateDelta,
}

/// `skill_listing` attachment. Emits the
/// delta between `available_skills` and the session's
/// `sent_skill_names` — on the first poll, lists everything; on
/// subsequent polls only the newly-registered skills. Both branches
/// share the same `- name: description` layout, capped at
/// [`SKILL_LISTING_NAME_CAP`] entries with an overflow notice.
pub fn skill_listing(input: &SkillListingInput) -> SkillListingOutput {
    let sent: HashSet<&str> = input
        .attachment_state
        .sent_skill_names
        .iter()
        .map(String::as_str)
        .collect();
    let new_skills: Vec<AvailableSkill> = input
        .available_skills
        .iter()
        .filter(|skill| !sent.contains(skill.name.as_str()))
        .cloned()
        .collect();
    if new_skills.is_empty() {
        return SkillListingOutput::default();
    }
    let is_initial = input.attachment_state.sent_skill_names.is_empty();
    let content = render_skill_listing(&new_skills, is_initial);
    let new_names: Vec<String> = new_skills.iter().map(|s| s.name.clone()).collect();
    SkillListingOutput {
        messages: vec![make_meta_user_message(&content)],
        state_delta: SkillListingStateDelta {
            add_sent_skill_names: new_names,
        },
    }
}

fn render_skill_listing(new_skills: &[AvailableSkill], is_initial: bool) -> String {
    let mut sorted = new_skills.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let header = if is_initial {
        SKILL_LISTING_INITIAL_HEADER
    } else {
        SKILL_LISTING_DELTA_HEADER
    };
    let mut lines = vec![header.to_string(), String::new()];
    lines.extend(
        sorted
            .iter()
            .take(SKILL_LISTING_NAME_CAP)
            .map(format_skill_listing_entry),
    );

    let overflow = sorted.len().saturating_sub(SKILL_LISTING_NAME_CAP);
    if overflow > 0 {
        lines.push(format!(
            "...and {overflow} more — invoke Skill with the skill id to load its body."
        ));
    }

    lines.join("\n")
}

fn format_skill_listing_entry(skill: &AvailableSkill) -> String {
    let description = skill.description.trim();
    if description.is_empty() {
        format!("- {}", skill.name)
    } else {
        format!("- {}: {}", skill.name, description)
    }
}

/// [`AttachmentPoller`] for one session's turn: the record it diffs against,
/// plus the catalogue handle the binding carried.
pub struct SkillAttachmentPoller {
    state: Arc<ServerState>,
    session_id: String,
    catalog: Arc<dyn TurnSkillCatalog>,
    /// What this turn offers the model. `None` offers nothing, so a skill
    /// with `required-tools` stays unlisted on a host that binds no toolkit.
    toolkit: Option<Arc<dyn TurnToolkit>>,
}

impl SkillAttachmentPoller {
    pub fn new(
        state: Arc<ServerState>,
        session_id: impl Into<String>,
        catalog: Arc<dyn TurnSkillCatalog>,
        toolkit: Option<Arc<dyn TurnToolkit>>,
    ) -> Self {
        Self {
            state,
            session_id: session_id.into(),
            catalog,
            toolkit,
        }
    }

    fn offered_skills(&self) -> Vec<AvailableSkill> {
        let has_tool = |name: &str| {
            self.toolkit
                .as_ref()
                .is_some_and(|toolkit| toolkit.has_tool(name))
        };
        self.catalog
            .available_skills()
            .into_iter()
            .filter(|skill| skill.is_offered_by(has_tool))
            .collect()
    }
}

impl AttachmentPoller for SkillAttachmentPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let Some(record) = self.state.attachment_session_snapshot(&self.session_id) else {
            return Vec::new();
        };

        let input = SkillListingInput {
            attachment_state: record.attachment_state.clone(),
            available_skills: self.offered_skills(),
        };

        let output = skill_listing(&input);

        if !output.state_delta.add_sent_skill_names.is_empty() {
            let _ = self.state.record_sent_skill_names(
                &self.session_id,
                &output.state_delta.add_sent_skill_names,
            );
        }

        output.messages
    }
}

/// The seat entry.
///
/// A host that binds no catalogue declines the turn: with nothing to list
/// there is no delta, and a poller that answers empty on every iteration is
/// worth less than not being in the turn at all.
pub struct SkillAttachmentProducer;

impl SeatAttachmentProducer for SkillAttachmentProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        let catalog = binding.skills.clone()?;
        Some(Arc::new(SkillAttachmentPoller::new(
            binding.state.clone(),
            binding.session_id.clone(),
            catalog,
            binding.toolkit.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(next_iteration: u64) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new(
            "session",
            "turn",
            next_iteration,
            AttachmentPollPhase::Regular,
        )
    }

    fn base_input() -> SkillListingInput {
        SkillListingInput {
            attachment_state: SessionAttachmentState::default(),
            available_skills: Vec::new(),
        }
    }

    fn skill(name: &str, description: &str) -> AvailableSkill {
        AvailableSkill::new(name, description)
    }

    fn only_text(messages: &[ApiMessage]) -> String {
        messages
            .iter()
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ApiContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    #[test]
    fn skill_listing_noop_when_no_skills() {
        let out = skill_listing(&base_input());
        assert!(out.messages.is_empty());
    }

    #[test]
    fn skill_listing_initial_lists_names_with_descriptions() {
        let mut input = base_input();
        input.available_skills = vec![
            skill("commit", "Generate a conventional commit message"),
            skill("review-pr", "Review a pull request"),
        ];
        let out = skill_listing(&input);
        let text = only_text(&out.messages);
        assert!(text.contains("The following skills are available via the Skill tool:"));
        assert!(text.contains("- commit: Generate a conventional commit message"));
        assert!(text.contains("- review-pr: Review a pull request"));
        // Initial branch should not use the "just registered" header.
        assert!(!text.contains("just registered"));
        assert_eq!(
            out.state_delta.add_sent_skill_names,
            vec!["commit".to_string(), "review-pr".to_string()]
        );
    }

    #[test]
    fn skill_listing_omits_description_separator_when_description_blank() {
        let mut input = base_input();
        input.available_skills = vec![skill("commit", ""), skill("review", "  ")];
        let out = skill_listing(&input);
        let text = only_text(&out.messages);
        assert!(text.contains("- commit\n"));
        assert!(text.lines().any(|line| line == "- review"));
        assert!(!text.contains("- commit:"));
        assert!(!text.contains("- review:"));
    }

    #[test]
    fn skill_listing_emits_only_delta_on_second_poll() {
        let mut first_input = base_input();
        first_input.available_skills = vec![
            skill("commit", "Generate a conventional commit message"),
            skill("lint", "Run the project linter"),
        ];
        let first = skill_listing(&first_input);
        let first_text = only_text(&first.messages);
        assert!(first_text.contains("The following skills are available via the Skill tool:"));
        assert!(first_text.contains("- commit: Generate a conventional commit message"));
        assert!(first_text.contains("- lint: Run the project linter"));

        let mut input = base_input();
        input.available_skills = vec![
            skill("commit", "Generate a conventional commit message"),
            skill("review-pr", "Review a pull request"),
            skill("lint", "Run the project linter"),
        ];
        input.attachment_state.sent_skill_names = first.state_delta.add_sent_skill_names;
        let out = skill_listing(&input);
        let text = only_text(&out.messages);
        assert!(text.contains("just registered"));
        assert!(text.contains("- review-pr: Review a pull request"));
        assert!(!text.contains("- commit"));
        assert!(!text.contains("- lint"));
        assert_eq!(
            out.state_delta.add_sent_skill_names,
            vec!["review-pr".to_string()]
        );
    }

    #[test]
    fn skill_listing_caps_at_fifteen_and_marks_all_sent() {
        let mut input = base_input();
        input.attachment_state.sent_skill_names = vec!["existing".into()];
        input.available_skills = std::iter::once(skill("existing", "already announced"))
            .chain((0..18).map(|idx| {
                AvailableSkill::new(
                    format!("new-skill-{idx:02}"),
                    format!("New skill number {idx}"),
                )
            }))
            .collect();

        let out = skill_listing(&input);
        let text = only_text(&out.messages);
        assert!(text.contains("just registered"));
        assert!(text.contains("...and 3 more — invoke Skill with the skill id to load its body."));
        assert_eq!(
            text.lines().filter(|line| line.starts_with("- ")).count(),
            SKILL_LISTING_NAME_CAP
        );
        for idx in 0..SKILL_LISTING_NAME_CAP {
            assert!(
                text.contains(&format!("- new-skill-{idx:02}: New skill number {idx}")),
                "missing entry new-skill-{idx:02}"
            );
        }
        for idx in SKILL_LISTING_NAME_CAP..18 {
            assert!(
                !text.contains(&format!("- new-skill-{idx:02}")),
                "should not list new-skill-{idx:02}"
            );
        }
        // The state delta still records every new skill so the next
        // poll won't re-announce truncated entries.
        assert_eq!(out.state_delta.add_sent_skill_names.len(), 18);
        assert!(out
            .state_delta
            .add_sent_skill_names
            .contains(&"new-skill-17".to_string()));
    }

    #[test]
    fn skill_listing_noop_when_all_announced() {
        let mut input = base_input();
        input.available_skills = vec![skill("commit", "Generate a commit message")];
        input.attachment_state.sent_skill_names = vec!["commit".into()];
        let out = skill_listing(&input);
        assert!(out.messages.is_empty());
    }

    /// Every message this producer emits must be one the engine's run loop
    /// can recognise, or a process restart re-sends the whole catalogue.
    #[test]
    fn both_headers_are_what_the_engine_recognises() {
        let mut initial = base_input();
        initial.available_skills = vec![skill("commit", "Generate a commit message")];
        let initial_text = only_text(&skill_listing(&initial).messages);
        assert!(rebon_core::attachments::is_skill_listing_text(
            &initial_text
        ));

        let mut delta = base_input();
        delta.available_skills = vec![skill("lint", "Run the linter")];
        delta.attachment_state.sent_skill_names = vec!["commit".into()];
        let delta_text = only_text(&skill_listing(&delta).messages);
        assert!(rebon_core::attachments::is_skill_listing_text(&delta_text));
    }

    // ── the seat entry ────────────────────────────────────────────

    struct FixedCatalog(Vec<AvailableSkill>);

    impl TurnSkillCatalog for FixedCatalog {
        fn available_skills(&self) -> Vec<AvailableSkill> {
            self.0.clone()
        }
    }

    fn session() -> (Arc<ServerState>, String) {
        let state = Arc::new(ServerState::new());
        let record = state.create_session("/tmp/skills".into(), Vec::new());
        let id = record.id.clone();
        (state, id)
    }

    /// A host that binds no catalogue is not in the turn at all.
    #[test]
    fn a_binding_without_a_catalogue_declines_the_turn() {
        let (state, id) = session();
        let binding = SessionAttachmentBinding::new(state, id);
        assert!(SkillAttachmentProducer
            .poller_for_session(&binding)
            .is_none());
    }

    /// The delta is remembered in the session record, so the second poll of
    /// the same catalogue says nothing and a grown catalogue says only what
    /// grew.
    #[test]
    fn the_poller_records_what_it_announced_and_only_emits_the_delta() {
        let (state, id) = session();

        let first = SkillAttachmentPoller::new(
            state.clone(),
            id.clone(),
            Arc::new(FixedCatalog(vec![skill("commit", "Generate a commit")])),
            None,
        );
        let messages = first.poll(request(0));
        assert_eq!(messages.len(), 1);
        assert!(
            only_text(&messages).contains("The following skills are available via the Skill tool:")
        );
        assert!(first.poll(request(1)).is_empty(), "nothing new to announce");

        let grown = SkillAttachmentPoller::new(
            state.clone(),
            id.clone(),
            Arc::new(FixedCatalog(vec![
                skill("commit", "Generate a commit"),
                skill("review-pr", "Review a pull request"),
            ])),
            None,
        );
        let second = only_text(&grown.poll(request(2)));
        assert!(second.contains("just registered"));
        assert!(second.contains("- review-pr: Review a pull request"));
        assert!(!second.contains("- commit"));

        let stored = state.get_session(&id).expect("session");
        assert_eq!(
            stored.attachment_state.sent_skill_names,
            vec!["commit".to_string(), "review-pr".to_string()]
        );
    }

    /// An unknown session id is a no-op rather than a panic.
    #[test]
    fn an_unknown_session_polls_to_nothing() {
        let poller = SkillAttachmentPoller::new(
            Arc::new(ServerState::new()),
            "sess-ghost",
            Arc::new(FixedCatalog(vec![skill("commit", "Generate a commit")])),
            None,
        );
        assert!(poller.poll(request(0)).is_empty());
    }

    // ── required tools ────────────────────────────────────────────

    struct Toolkit(&'static [&'static str]);

    impl TurnToolkit for Toolkit {
        fn has_tool(&self, name: &str) -> bool {
            self.0.contains(&name)
        }
    }

    fn imagegen() -> AvailableSkill {
        skill("imagegen", "Generate images").with_required_tools(vec!["ImageGen".into()])
    }

    fn listed(toolkit: Option<Arc<dyn TurnToolkit>>) -> String {
        let (state, id) = session();
        let poller = SkillAttachmentPoller::new(
            state,
            id,
            Arc::new(FixedCatalog(vec![
                skill("commit", "Generate a commit"),
                imagegen(),
            ])),
            toolkit,
        );
        only_text(&poller.poll(request(0)))
    }

    /// The turn offers the tool the skill drives, so the skill is named.
    #[test]
    fn a_skill_is_listed_when_the_turn_offers_every_required_tool() {
        let text = listed(Some(Arc::new(Toolkit(&["Read", "ImageGen"]))));
        assert!(text.contains("- imagegen: Generate images"), "{text}");
        assert!(text.contains("- commit: Generate a commit"), "{text}");
    }

    /// Without the tool the skill is instructions for a call the model
    /// cannot make; skills with no requirement are unaffected.
    #[test]
    fn a_skill_is_withheld_when_a_required_tool_is_missing() {
        let text = listed(Some(Arc::new(Toolkit(&["Read"]))));
        assert!(!text.contains("imagegen"), "{text}");
        assert!(text.contains("- commit: Generate a commit"), "{text}");
    }

    /// A host that binds no toolkit offers no tools, so a requirement is
    /// never met there.
    #[test]
    fn a_binding_without_a_toolkit_withholds_skills_that_need_tools() {
        let text = listed(None);
        assert!(!text.contains("imagegen"), "{text}");
        assert!(text.contains("- commit"), "{text}");
    }

    /// A withheld skill is not recorded as sent, so it is announced on the
    /// first poll whose turn does offer the tool — a `/model` switch onto a
    /// provider that has it.
    #[test]
    fn a_withheld_skill_is_announced_once_its_tool_appears() {
        let (state, id) = session();
        let catalog: Arc<dyn TurnSkillCatalog> = Arc::new(FixedCatalog(vec![imagegen()]));
        let without = SkillAttachmentPoller::new(state.clone(), id.clone(), catalog.clone(), None);
        assert!(without.poll(request(0)).is_empty());
        assert!(state
            .get_session(&id)
            .expect("session")
            .attachment_state
            .sent_skill_names
            .is_empty());

        let with =
            SkillAttachmentPoller::new(state, id, catalog, Some(Arc::new(Toolkit(&["ImageGen"]))));
        assert!(only_text(&with.poll(request(1))).contains("- imagegen"));
    }

    #[test]
    fn every_required_tool_must_be_offered() {
        let skill = skill("both", "").with_required_tools(vec!["ImageGen".into(), "Read".into()]);
        assert!(skill.is_offered_by(|name| name == "ImageGen" || name == "Read"));
        assert!(!skill.is_offered_by(|name| name == "ImageGen"));
        assert!(AvailableSkill::new("free", "").is_offered_by(|_| false));
    }
}
