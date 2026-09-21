//! Kernel-backed attachment-production seat.
//!
//! The engine's query loop sees one [`AttachmentPoller`] per turn, however
//! many things want to speak into it. A producer that belongs to a feature has
//! no business being wired into the engine, so this seat is where a plugin
//! puts one instead. Two of them are nobody's feature — a calendar
//! (`date_change`) and a queued prompt (`runtime_prompts`) — and those are
//! registered by `core-tools`, reaching a turn through this seat like
//! everything else.
//!
//! Shape. The seat is a **process-level** registry: a plugin registers once,
//! on its own context, and the binding to a session arrives later. It has to
//! be that way round, because the two things a session-state producer reads
//! from — the [`ServerState`] and the session id — are the executor's, not the
//! kernel's: `SessionOpened` carries a scope and an id and no state. So a
//! producer is asked, once per turn, for the poller it wants for *this*
//! session ([`SeatAttachmentProducer::poller_for_session`]), and may answer
//! `None` to stay out of that turn entirely.
//!
//! Order. Producers run in [`Order`] then provider-id order, and that sequence
//! is the whole of it: no engine-owned poller sits in the middle for the rungs
//! to be measured against. Each rung names what belongs there, so what the
//! model reads does not depend on which plugins loaded or when: plan mode's
//! reminders at
//! [`Order::Transition`], the date roll at [`Order::DayRoll`], the skill
//! listing at [`Order::Listing`], nested memory at [`Order::Context`], the
//! queued prompts at [`Order::Prompt`], the teammate mailbox at
//! [`Order::Mailbox`], the task nudge at [`Order::Reminder`]. Two producers
//! from one plugin are ordered by their rungs, never by the order they
//! registered.
//!
//! Lifetime. A registration is an effect of the registering context: unload
//! the plugin and the producer leaves the seat, and a poller already handed to
//! a turn in flight goes quiet rather than outliving its provider.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebon_api::Message as ApiMessage;
use rebon_kernel::{Context, Disposer, KernelError, Service};
use rebon_session_state::ServerState;

#[cfg(test)]
use crate::query::AttachmentPollPhase;
use crate::query::{AttachmentPollRequest, AttachmentPoller};

pub const ATTACHMENT_SEAT_SERVICE: &str = "attachment-producers";

/// Where a producer stands in one poll's output: the lowest value speaks
/// first.
///
/// The rungs are the eight-producer fixed order the engine used to run
/// internally, named for what a producer *is* rather than for whichever
/// producer holds the rung today, so a new one can find its place without
/// asking what came before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Order {
    /// One-shot notices about a transition the session just made — the
    /// model has to read these before anything that describes the new
    /// state. `plan_mode_exit` and the `plan_mode` reminder are here.
    Transition = 0,
    /// The world moved under the session while it was mid-turn and nothing
    /// it did caused it: the calendar rolled over. `date_change` is here.
    DayRoll = 25,
    /// Deltas of a catalogue the session can draw on (skills, agents).
    Listing = 50,
    /// Documents the session just gained and has to read before it acts on
    /// them — a nested `CLAUDE.md` a tool walked into, an edited `REBON.md`.
    /// They are context, not news, so they follow the catalogues.
    Context = 60,
    /// Prompts queued for the session from outside the model loop, replayed
    /// verbatim. They read as if the user had said them, so they come after
    /// everything that describes the state they will be read against.
    Prompt = 70,
    /// Traffic that arrived from outside the session while the turn was in
    /// flight — the teammate mailbox. It follows what the session already
    /// knows about itself and precedes anything that merely nags.
    Mailbox = 75,
    /// Recurring nudges that only make sense once the rest is said.
    Reminder = 100,
}

impl Order {
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// Which file-backed inbox a turn drains.
///
/// Lives here rather than with the producer that reads it because the
/// executor is what resolves it — the in-process `TeamManager` first, the
/// team files second — and hands it over on the binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxIdentity {
    /// Team name for the session or teammate whose inbox is being drained.
    pub team_name: String,
    /// Agent name — `"team-lead"` for the lead process, or the teammate's
    /// registered agent name for a teammate loop.
    pub agent_name: String,
}

/// The task list a turn's producers read from.
///
/// One method, resolved per poll rather than captured at bind time, because
/// the list a session belongs to can change inside a turn: a `TeamCreate` in
/// the first tool round moves the session onto the team's list.
pub trait TurnTaskList: Send + Sync {
    /// Id of the list this session's tasks live in.
    fn task_list_id(&self) -> String;
}

/// The tools this turn actually offers the model.
///
/// The set is the executor's `effective_tool_filter` applied to the engine's
/// toolkit, which is what decides whether a producer whose subject is a tool
/// has anything to talk about.
pub trait TurnToolkit: Send + Sync {
    fn has_tool(&self, name: &str) -> bool;
}

/// One skill registered in the session, captured for the `skill_listing`
/// attachment. `name` is the unique invocation id; `description` is the short
/// single-line summary surfaced in the listing so the model can decide whether
/// to load the body via the Skill tool.
///
/// Lives here rather than with the producer that renders it for the same
/// reason [`MailboxIdentity`] does: it is what the binding carries, and the
/// executor is what fills it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSkill {
    pub name: String,
    pub description: String,
    /// Tools the turn must offer before the skill is named: its frontmatter
    /// `required-tools`. Empty for a skill that stands on its own.
    pub required_tools: Vec<String>,
}

impl AvailableSkill {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            required_tools: Vec::new(),
        }
    }

    pub fn with_required_tools(mut self, required_tools: Vec<String>) -> Self {
        self.required_tools = required_tools;
        self
    }

    /// Whether a turn whose toolkit answers `has_tool` offers every tool this
    /// skill drives. A skill written around one tool — `imagegen` around
    /// `ImageGen` — is only instructions for a call the model cannot make
    /// when the tool is absent, so it is not worth a listing line.
    pub fn is_offered_by(&self, has_tool: impl Fn(&str) -> bool) -> bool {
        self.required_tools.iter().all(|tool| has_tool(tool))
    }
}

/// The skills this turn's session can invoke.
///
/// Resolved per poll, not captured at bind time, because the catalogue grows
/// during a turn: the progressive-discovery hook registers a skill it found
/// under a path a tool just touched, and the listing has to name it on the
/// next round.
pub trait TurnSkillCatalog: Send + Sync {
    fn available_skills(&self) -> Vec<AvailableSkill>;

    /// Read a `/<name> args` prompt as an invocation of one of these skills,
    /// when it names one the user is allowed to invoke.
    ///
    /// The same catalogue answers both questions because they are the same
    /// question — what can this turn invoke — and answering them from two
    /// handles would let a name be listed and not resolvable, or the reverse.
    /// The executor asks only when its caller resolved nothing itself, so a
    /// front end that already reported disabled and model-only skills to the
    /// user stays authoritative. `None` for anything else, which is what
    /// keeps `/help` in a conversation about documentation ordinary text.
    ///
    /// Defaulted to `None` for a host that hands over a fixed listing and has
    /// no index to resolve against.
    fn typed_invocation(
        &self,
        _user_text: &str,
    ) -> Option<rebon_agent_core::SkillInvocationRequest> {
        None
    }
}

/// One nested-memory trigger, ready to be rendered as an attachment.
///
/// The engine finds these — `query::session_prompt` diffs the document
/// snapshot at turn start — and the producer that renders them is the memory
/// plugin's, so the type is the binding's payload like the others here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedMemoryTrigger {
    /// Display path — used unchanged in the attachment header.
    pub display_path: String,
    /// File contents to inline.
    pub content: String,
}

/// The documents this turn found changed under it.
///
/// Draining, not snapshotting: a trigger is delivered once. The executor fills
/// the handle per turn, so what a poller drains is that turn's finds.
pub trait TurnDocumentTriggers: Send + Sync {
    fn drain_document_triggers(&self) -> Vec<NestedMemoryTrigger>;
}

/// The reusable teammates this session can dispatch to.
///
/// A non-draining snapshot: it becomes request-scoped transient context rather
/// than durable history, so the query loop may ask for it before every model
/// request and must see the current answer each time.
pub trait TurnTeammateRoster: Send + Sync {
    fn teammate_roster(&self) -> Vec<rebon_tool::TeammateRosterEntry>;
}

/// The inbox this turn's session drains.
///
/// Resolved per poll, not captured at bind time, for the same reason the task
/// list is: a `TeamCreate` in the first tool round gives a session a team it
/// did not have when the turn started, and the traffic has to reach it in the
/// same turn.
pub trait TurnMailbox: Send + Sync {
    /// The identity to drain, or `None` when the session has no team.
    fn mailbox_identity(&self) -> Option<MailboxIdentity>;
}

/// Everything a session-state attachment producer needs to bind itself to one
/// turn's session.
///
/// The state and the id are the floor: what a producer reading the session
/// record needs, and all the engine can promise on every host. Everything past
/// them is one **optional** fine-grained handle per thing a producer reads
/// outside the record — a host that has no such thing leaves it `None` and the
/// producer sees the same "nothing to say" it would have seen from the
/// engine's own defaults. A producer asks for the one input it reads; there
/// is deliberately no single grab-bag a plugin can reach the whole turn
/// through.
pub struct SessionAttachmentBinding {
    pub state: Arc<ServerState>,
    pub session_id: String,
    /// Resolves the id of the task list this session's tasks live in.
    pub task_list: Option<Arc<dyn TurnTaskList>>,
    /// Answers what this turn's toolkit offers the model.
    pub toolkit: Option<Arc<dyn TurnToolkit>>,
    /// Resolves the inbox this turn drains.
    pub mailbox: Option<Arc<dyn TurnMailbox>>,
    /// Resolves the skills this turn can invoke.
    pub skills: Option<Arc<dyn TurnSkillCatalog>>,
    /// Drains the documents this turn found changed under it.
    pub documents: Option<Arc<dyn TurnDocumentTriggers>>,
    /// Resolves the reusable teammates this session can dispatch to.
    pub roster: Option<Arc<dyn TurnTeammateRoster>>,
}

impl SessionAttachmentBinding {
    /// The floor binding: the record, and no handles. Every host can build
    /// this one.
    pub fn new(state: Arc<ServerState>, session_id: impl Into<String>) -> Self {
        Self {
            state,
            session_id: session_id.into(),
            task_list: None,
            toolkit: None,
            mailbox: None,
            skills: None,
            documents: None,
            roster: None,
        }
    }

    pub fn with_task_list(mut self, task_list: Arc<dyn TurnTaskList>) -> Self {
        self.task_list = Some(task_list);
        self
    }

    pub fn with_toolkit(mut self, toolkit: Arc<dyn TurnToolkit>) -> Self {
        self.toolkit = Some(toolkit);
        self
    }

    pub fn with_mailbox(mut self, mailbox: Arc<dyn TurnMailbox>) -> Self {
        self.mailbox = Some(mailbox);
        self
    }

    pub fn with_skills(mut self, skills: Arc<dyn TurnSkillCatalog>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn with_documents(mut self, documents: Arc<dyn TurnDocumentTriggers>) -> Self {
        self.documents = Some(documents);
        self
    }

    pub fn with_roster(mut self, roster: Arc<dyn TurnTeammateRoster>) -> Self {
        self.roster = Some(roster);
        self
    }

    /// The documents this turn found changed, drained. Empty on a host that
    /// binds none.
    pub fn drain_document_triggers(&self) -> Vec<NestedMemoryTrigger> {
        self.documents
            .as_ref()
            .map(|documents| documents.drain_document_triggers())
            .unwrap_or_default()
    }

    /// This session's reusable teammates. Empty on a host that binds none,
    /// and on a solo session.
    pub fn teammate_roster(&self) -> Vec<rebon_tool::TeammateRosterEntry> {
        self.roster
            .as_ref()
            .map(|roster| roster.teammate_roster())
            .unwrap_or_default()
    }

    /// The skills this turn can invoke. Empty on a host that binds no
    /// catalogue, which is the answer the engine's own sources gave by
    /// default.
    pub fn available_skills(&self) -> Vec<AvailableSkill> {
        self.skills
            .as_ref()
            .map(|skills| skills.available_skills())
            .unwrap_or_default()
    }

    /// The inbox this turn drains, or `None` on a host that binds no mailbox
    /// and for a session with no team.
    pub fn mailbox_identity(&self) -> Option<MailboxIdentity> {
        self.mailbox
            .as_ref()
            .and_then(|mailbox| mailbox.mailbox_identity())
    }

    /// The turn's task list id, or `None` on a host that binds no task list.
    pub fn task_list_id(&self) -> Option<String> {
        self.task_list.as_ref().map(|list| list.task_list_id())
    }

    /// Whether this turn offers `name` to the model. `false` without a bound
    /// toolkit, which is the answer the engine's own sources gave by default.
    pub fn has_tool(&self, name: &str) -> bool {
        self.toolkit
            .as_ref()
            .is_some_and(|toolkit| toolkit.has_tool(name))
    }
}

/// One source of attachments behind the seat.
///
/// Asked once per turn. Returning `None` keeps the producer out of that turn's
/// composite entirely, which is cheaper than a poller that returns an empty
/// vector every iteration.
pub trait SeatAttachmentProducer: Send + Sync {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>>;
}

struct ProducerEntry {
    id: String,
    order: u16,
    token: u64,
    active: Arc<AtomicBool>,
    producer: Arc<dyn SeatAttachmentProducer>,
}

/// Typed definition for the kernel's `attachment-producers` seat.
///
/// Like the tool seat, the interface is the seat itself rather than a `dyn`
/// consumer half: a feature plugin registers onto it and the engine's
/// executor reads producers off it, and both faces are needed through the
/// kernel.
pub struct AttachmentSeatService;

impl Service for AttachmentSeatService {
    type Interface = AttachmentSeat;
    const NAME: &'static str = ATTACHMENT_SEAT_SERVICE;
}

/// Producer registry behind the typed `attachment-producers` service.
pub struct AttachmentSeat {
    entries: RwLock<Vec<ProducerEntry>>,
    next_token: AtomicU64,
}

impl AttachmentSeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(1),
        })
    }

    /// Register a producer on `ctx`. The registration is an effect of the
    /// context: when `ctx` is disposed (the plugin unloads) the producer
    /// leaves the seat and pollers already handed to turns in flight fall
    /// silent. `id` must be unique within the seat.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        order: Order,
        producer: Arc<dyn SeatAttachmentProducer>,
    ) -> Result<(), KernelError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(KernelError::Other(
                "attachment-producers provider id must be non-empty".into(),
            ));
        }
        let order = order.as_u16();
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let active = Arc::new(AtomicBool::new(true));
        {
            let mut entries = self.entries.write().expect("attachment seat poisoned");
            if entries.iter().any(|entry| entry.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: format!("{ATTACHMENT_SEAT_SERVICE}:{id}"),
                });
            }
            entries.push(ProducerEntry {
                id: id.to_string(),
                order,
                token,
                active: active.clone(),
                producer,
            });
            entries.sort_by(|left, right| {
                left.order
                    .cmp(&right.order)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }

        let weak = Arc::downgrade(self);
        let id_for_dispose = id.to_string();
        ctx.effect_labeled(&format!("attachment producer({id})"), move || {
            Disposer::new(move || {
                active.store(false, Ordering::Release);
                if let Some(seat) = weak.upgrade() {
                    seat.entries
                        .write()
                        .expect("attachment seat poisoned")
                        .retain(|entry| !(entry.id == id_for_dispose && entry.token == token));
                }
            })
        });
        Ok(())
    }

    /// Provider ids currently on the seat, in poll order.
    pub fn provider_ids(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("attachment seat poisoned")
            .iter()
            .filter(|entry| entry.active.load(Ordering::Acquire))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// The pollers this session's turn should run, in poll order. Each is
    /// guarded: a producer unloaded mid-turn stops contributing instead of
    /// outliving its plugin.
    pub fn pollers_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Vec<Arc<dyn AttachmentPoller>> {
        let entries: Vec<(Arc<AtomicBool>, Arc<dyn SeatAttachmentProducer>)> = self
            .entries
            .read()
            .expect("attachment seat poisoned")
            .iter()
            .map(|entry| (entry.active.clone(), entry.producer.clone()))
            .collect();
        entries
            .into_iter()
            .filter(|(active, _)| active.load(Ordering::Acquire))
            .filter_map(|(active, producer)| {
                producer.poller_for_session(binding).map(|poller| {
                    Arc::new(GuardedPoller { poller, active }) as Arc<dyn AttachmentPoller>
                })
            })
            .collect()
    }
}

/// The seat's pollers for `binding`, when the kernel scope `ctx` can see an
/// `attachment-producers` seat. Without one — a host with no kernel, a test
/// engine — the answer is an empty list and the turn injects nothing, which is
/// the same silence such a host got before any of this moved.
pub fn pollers_for_session(
    ctx: &Context,
    binding: &SessionAttachmentBinding,
) -> Vec<Arc<dyn AttachmentPoller>> {
    match ctx.get::<AttachmentSeatService>() {
        Some(seat) => seat.pollers_for_session(binding),
        None => Vec::new(),
    }
}

/// A seat poller that goes quiet when its provider leaves the seat.
///
/// Only the producing half is gated. The notification half
/// (`notify_plan_mode_tool`, `finish_turn`) still forwards: a tool call that
/// already ran has to land in the session record whether or not the plugin
/// survived the round trip.
struct GuardedPoller {
    poller: Arc<dyn AttachmentPoller>,
    active: Arc<AtomicBool>,
}

impl GuardedPoller {
    fn live(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

impl AttachmentPoller for GuardedPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if self.live() {
            self.poller.poll(request)
        } else {
            Vec::new()
        }
    }

    fn transient_context(&self) -> Option<String> {
        self.live()
            .then(|| self.poller.transient_context())
            .flatten()
    }

    fn transient_context_for_turn(&self, turn_id: &str) -> Option<String> {
        self.live()
            .then(|| self.poller.transient_context_for_turn(turn_id))
            .flatten()
    }

    fn transient_context_for_query(&self, session_id: &str, turn_id: &str) -> Option<String> {
        self.live()
            .then(|| self.poller.transient_context_for_query(session_id, turn_id))
            .flatten()
    }

    fn finish_turn(&self, succeeded: bool) {
        self.poller.finish_turn(succeeded);
    }

    fn finish_turn_for_turn(&self, turn_id: &str, succeeded: bool) {
        self.poller.finish_turn_for_turn(turn_id, succeeded);
    }

    fn finish_turn_for_query(&self, session_id: &str, turn_id: &str, succeeded: bool) {
        self.poller
            .finish_turn_for_query(session_id, turn_id, succeeded);
    }

    fn take_coordinator_report_paths(&self) -> Vec<std::path::PathBuf> {
        self.poller.take_coordinator_report_paths()
    }

    fn take_coordinator_report_paths_for_turn(&self, turn_id: &str) -> Vec<std::path::PathBuf> {
        self.poller.take_coordinator_report_paths_for_turn(turn_id)
    }

    fn take_coordinator_report_paths_for_query(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Vec<std::path::PathBuf> {
        self.poller
            .take_coordinator_report_paths_for_query(session_id, turn_id)
    }

    fn notify_task_tool_used(&self, iteration: u64) {
        self.poller.notify_task_tool_used(iteration);
    }

    fn notify_plan_mode_tool(
        &self,
        tool_name: &str,
        succeeded: bool,
        tool_result: Option<&serde_json::Value>,
    ) {
        self.poller
            .notify_plan_mode_tool(tool_name, succeeded, tool_result);
    }

    fn take_context_reset(&self) -> Option<Vec<ApiMessage>> {
        self.poller.take_context_reset()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    struct Fixed {
        tag: &'static str,
    }

    struct Tagged {
        tag: &'static str,
    }

    impl AttachmentPoller for Tagged {
        fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
            vec![ApiMessage::user_text(self.tag)]
        }
    }

    impl SeatAttachmentProducer for Fixed {
        fn poller_for_session(
            &self,
            _binding: &SessionAttachmentBinding,
        ) -> Option<Arc<dyn AttachmentPoller>> {
            Some(Arc::new(Tagged { tag: self.tag }))
        }
    }

    struct Absent;

    impl SeatAttachmentProducer for Absent {
        fn poller_for_session(
            &self,
            _binding: &SessionAttachmentBinding,
        ) -> Option<Arc<dyn AttachmentPoller>> {
            None
        }
    }

    fn binding() -> SessionAttachmentBinding {
        SessionAttachmentBinding::new(Arc::new(ServerState::new()), "sess-1")
    }

    fn request(next_iteration: u64, phase: AttachmentPollPhase) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new("sess-1", "turn-1", next_iteration, phase)
    }

    fn texts(pollers: &[Arc<dyn AttachmentPoller>]) -> Vec<String> {
        collect(pollers, |poller| {
            poller.poll(request(1, AttachmentPollPhase::Regular))
        })
    }

    fn collect(
        pollers: &[Arc<dyn AttachmentPoller>],
        ask: impl Fn(&Arc<dyn AttachmentPoller>) -> Vec<ApiMessage>,
    ) -> Vec<String> {
        pollers
            .iter()
            .flat_map(ask)
            .flat_map(|message| {
                message.content.into_iter().filter_map(|block| match block {
                    rebon_api::ContentBlock::Text(text) => Some(text.text),
                    _ => None,
                })
            })
            .collect()
    }

    /// The eager poll — the one the query loop runs before the turn's first
    /// model request — reaches seat pollers, and stops reaching one whose
    /// provider left. Nested memory rides this path, so if it ever stopped
    /// being asked the mid-session `REBON.md` edit would land a round late.
    #[test]
    fn the_eager_poll_reaches_the_seat_and_falls_silent_with_its_provider() {
        struct Eager;
        impl AttachmentPoller for Eager {
            fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
                match request.phase {
                    AttachmentPollPhase::Regular => Vec::new(),
                    AttachmentPollPhase::Eager => vec![ApiMessage::user_text("eager")],
                }
            }
        }
        struct EagerProducer;
        impl SeatAttachmentProducer for EagerProducer {
            fn poller_for_session(
                &self,
                _binding: &SessionAttachmentBinding,
            ) -> Option<Arc<dyn AttachmentPoller>> {
                Some(Arc::new(Eager))
            }
        }

        let kernel = Kernel::new();
        let seat = AttachmentSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(&ctx, "memory", Order::Context, Arc::new(EagerProducer))
            .unwrap();

        let pollers = seat.pollers_for_session(&binding());
        assert!(texts(&pollers).is_empty(), "nothing on the round poll");
        assert_eq!(
            collect(&pollers, |poller| {
                poller.poll(request(0, AttachmentPollPhase::Eager))
            }),
            vec!["eager"]
        );

        ctx.dispose();
        assert!(collect(&pollers, |poller| {
            poller.poll(request(0, AttachmentPollPhase::Eager))
        })
        .is_empty());
    }

    /// Order is the rung first, the provider id second — registration
    /// order does not leak into what the model reads.
    #[test]
    fn producers_poll_in_rung_then_id_order() {
        let kernel = Kernel::new();
        let seat = AttachmentSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "zeta",
            Order::Transition,
            Arc::new(Fixed { tag: "zeta" }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "alpha",
            Order::Reminder,
            Arc::new(Fixed { tag: "alpha" }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "beta",
            Order::Transition,
            Arc::new(Fixed { tag: "beta" }),
        )
        .unwrap();

        let pollers = seat.pollers_for_session(&binding());
        assert_eq!(texts(&pollers), vec!["beta", "zeta", "alpha"]);
        assert_eq!(seat.provider_ids(), vec!["beta", "zeta", "alpha"]);
    }

    /// The whole of the original eight-producer order is now the seat's
    /// order, rung by rung, whatever sequence the plugins registered in.
    #[test]
    fn the_rungs_reproduce_the_engines_original_producer_order() {
        let kernel = Kernel::new();
        let seat = AttachmentSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "plan-mode",
            Order::Transition,
            Arc::new(Fixed { tag: "plan" }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "skills",
            Order::Listing,
            Arc::new(Fixed { tag: "skills" }),
        )
        .unwrap();
        // Registered before the mailbox, and still last: two producers from
        // one plugin are ordered by rung, not by when they registered.
        seat.register(
            &ctx,
            "tasks/reminder",
            Order::Reminder,
            Arc::new(Fixed { tag: "nudge" }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "tasks/mailbox",
            Order::Mailbox,
            Arc::new(Fixed { tag: "mailbox" }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "core/date-roll",
            Order::DayRoll,
            Arc::new(Fixed { tag: "date" }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "core/runtime-prompts",
            Order::Prompt,
            Arc::new(Fixed { tag: "prompts" }),
        )
        .unwrap();
        seat.register(
            &ctx,
            "memory",
            Order::Context,
            Arc::new(Fixed { tag: "memory" }),
        )
        .unwrap();

        assert_eq!(
            texts(&seat.pollers_for_session(&binding())),
            vec!["plan", "date", "skills", "memory", "prompts", "mailbox", "nudge"]
        );
    }

    /// A binding carries the record alone until a host adds handles, and a
    /// producer reading a handle that is not there sees the engine's old
    /// defaults: no task list, no tools, no inbox.
    #[test]
    fn handles_are_optional_and_absent_ones_read_as_nothing() {
        struct List;
        impl TurnTaskList for List {
            fn task_list_id(&self) -> String {
                "team-alpha".into()
            }
        }
        struct Toolkit;
        impl TurnToolkit for Toolkit {
            fn has_tool(&self, name: &str) -> bool {
                name == "TaskCreate"
            }
        }
        struct Inbox;
        impl TurnMailbox for Inbox {
            fn mailbox_identity(&self) -> Option<MailboxIdentity> {
                Some(MailboxIdentity {
                    team_name: "team-alpha".into(),
                    agent_name: "team-lead".into(),
                })
            }
        }

        struct Docs;
        impl TurnDocumentTriggers for Docs {
            fn drain_document_triggers(&self) -> Vec<NestedMemoryTrigger> {
                vec![NestedMemoryTrigger {
                    display_path: "src/CLAUDE.md".into(),
                    content: "hello".into(),
                }]
            }
        }

        let bare = binding();
        assert_eq!(bare.task_list_id(), None);
        assert!(!bare.has_tool("TaskCreate"));
        assert_eq!(bare.mailbox_identity(), None);
        assert!(bare.available_skills().is_empty());
        assert!(bare.drain_document_triggers().is_empty());
        assert!(bare.teammate_roster().is_empty());

        let bound = binding()
            .with_task_list(Arc::new(List))
            .with_toolkit(Arc::new(Toolkit))
            .with_mailbox(Arc::new(Inbox))
            .with_documents(Arc::new(Docs));
        assert_eq!(bound.task_list_id().as_deref(), Some("team-alpha"));
        assert!(bound.has_tool("TaskCreate"));
        assert!(!bound.has_tool("TaskUpdate"));
        assert_eq!(
            bound.mailbox_identity().map(|id| id.agent_name),
            Some("team-lead".to_string())
        );
        assert_eq!(bound.drain_document_triggers().len(), 1);
    }

    /// A producer that declines this session is not in the turn at all.
    #[test]
    fn a_producer_that_declines_a_session_is_left_out() {
        let kernel = Kernel::new();
        let seat = AttachmentSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(&ctx, "absent", Order::Transition, Arc::new(Absent))
            .unwrap();
        seat.register(
            &ctx,
            "present",
            Order::Listing,
            Arc::new(Fixed { tag: "here" }),
        )
        .unwrap();

        let pollers = seat.pollers_for_session(&binding());
        assert_eq!(pollers.len(), 1);
        assert_eq!(texts(&pollers), vec!["here"]);
    }

    /// Disposing the registering scope takes the producer off the seat, and
    /// a poller a turn is already holding falls silent.
    #[test]
    fn disposing_the_scope_silences_a_poller_already_handed_out() {
        let kernel = Kernel::new();
        let seat = AttachmentSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "plan-mode",
            Order::Transition,
            Arc::new(Fixed { tag: "x" }),
        )
        .unwrap();
        let pollers = seat.pollers_for_session(&binding());
        assert_eq!(texts(&pollers), vec!["x"]);

        ctx.dispose();

        assert!(seat.provider_ids().is_empty());
        assert!(texts(&pollers).is_empty());
        assert!(seat.pollers_for_session(&binding()).is_empty());
    }

    /// Two registrations under one id is a programming error, not a
    /// last-one-wins.
    #[test]
    fn a_duplicate_provider_id_is_refused() {
        let kernel = Kernel::new();
        let seat = AttachmentSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(&ctx, "dup", Order::Transition, Arc::new(Fixed { tag: "a" }))
            .unwrap();
        assert!(seat
            .register(&ctx, "dup", Order::Transition, Arc::new(Fixed { tag: "b" }))
            .is_err());
    }

    /// A scope with no seat above it answers "no producers" rather than
    /// failing the turn.
    #[test]
    fn a_scope_without_a_seat_contributes_nothing() {
        let kernel = Kernel::new();
        assert!(pollers_for_session(kernel.context(), &binding()).is_empty());
    }

    /// The seat is found from a session scope forked under the one that
    /// provides it — both fork kinds, because that is the shape the hosts
    /// hand the executor: `SessionKernelScopes` forks the process kernel
    /// with `fork_scoped`, and the seat lives on the root that `core-tools`
    /// provided it to.
    #[test]
    fn a_session_scope_under_the_providing_scope_sees_the_seat() {
        let kernel = Kernel::new();
        let seat = AttachmentSeat::new();
        kernel
            .context()
            .provide::<AttachmentSeatService>(seat.clone())
            .unwrap();
        seat.register(
            kernel.context(),
            "plan-mode",
            Order::Transition,
            Arc::new(Fixed { tag: "seen" }),
        )
        .unwrap();

        let plain = kernel.context().fork("session/abc");
        assert_eq!(
            texts(&pollers_for_session(&plain, &binding())),
            vec!["seen"]
        );

        let scoped = kernel.context().fork_scoped("session/def");
        assert_eq!(
            texts(&pollers_for_session(&scoped, &binding())),
            vec!["seen"]
        );
    }
}
