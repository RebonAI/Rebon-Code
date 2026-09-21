//! Kernel-backed system-prompt section seat.
//!
//! The engine assembles one system prompt per turn out of a fixed table of
//! sections, and a plugin that wants a say in that prompt puts a section on
//! this seat. Before the seat, the only way in was the Node composition's
//! `ctx.systemPrompt` registrations, mirrored through a process slot the
//! plane installed at boot: every contributed section landed on the stable
//! runtime-context plane, no Rust plugin could contribute at all, and the
//! table could not ask who the prompt was for. The seat is the same shape as
//! [`crate::attachment_seat`], and closes those three gaps.
//!
//! Shape. The seat is a **process-level** registry: a plugin registers a
//! provider once, on its own context, and is asked once per turn what
//! sections it wants for *this* turn's [`PromptSubject`] — the model the turn
//! speaks to, the session and workspace it speaks for, and the tools it
//! offers. An empty answer keeps the provider out of that turn's prompt, the
//! same way an attachment producer answering `None` stays out of a poll. A
//! provider whose section quotes files verbatim also names them
//! ([`PromptSectionProvider::injected_files`]) so the turn can seed the
//! read-state cache with what the model has already been shown.
//!
//! Placement. A section names a [`Rung`], and the rung says both which plane
//! it renders on and where in that plane it stands. The rungs on the base
//! plane are the places the engine's own sections hold, named for what a
//! section *is* rather than for whoever holds the rung today, so a section
//! that belongs next to the engine's tone guidance says [`Rung::Style`] and
//! does not need to know what came before. [`Rung::Context`] is where every
//! composition section used to land, so a Node plugin's registration renders
//! exactly where it did.
//!
//! Lifetime. A registration is an effect of the registering context: unload
//! the plugin and its provider leaves the seat, and the next turn's prompt is
//! assembled without it. The session prompt cache keys on the contributed
//! sections themselves (`query::session_prompt`), so a register or unregister
//! mid-session re-renders the frozen planes rather than serving stale bytes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebon_kernel::{Context, Disposer, KernelError, Service};

use crate::system_prompt::PromptPlane;

pub const PROMPT_SEAT_SERVICE: &str = "prompt-sections";

/// Where a contributed section stands in the assembled prompt.
///
/// Each rung is one plane plus one rank inside it. The base rungs mirror the
/// engine's own base table (`system_prompt::assembly`), so a plugin section
/// on a base rung renders right after the engine section that holds the same
/// rank, or in its place when the engine has no section at that rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rung {
    /// Who the model is (base plane, the engine's `intro`).
    Intro,
    /// The system the model runs in (base plane, the engine's `system`).
    System,
    /// How to carry out tasks (base plane, the engine's `doing-tasks`).
    Task,
    /// Which actions need care (base plane, the engine's `actions`).
    Actions,
    /// How to find tools (base plane, the engine's `tool-discovery`).
    ToolDiscovery,
    /// Tone and style preferences (base plane, the engine's `tone-and-style`).
    Style,
    /// Output economy (base plane, the engine's `output-efficiency`).
    Efficiency,
    /// Documents rebon loaded on the user's behalf and puts in front of the
    /// model verbatim, between the project instruction files and the MCP
    /// server instructions (stable runtime-context plane, the engine's
    /// `memory`).
    ///
    /// Its one holder is the `memory` plugin's auto-`MEMORY.md` section. The
    /// rung is named for the place rather than the holder, and the engine
    /// treats what stands here as *loaded document* content: a mid-session
    /// change to it is announced through the nested-memory channel
    /// (`query::session_prompt`), and the files behind it are pre-seeded into
    /// the read-state cache through [`PromptSectionProvider::injected_files`].
    Memory,
    /// Low-churn runtime context, after rebon's own tool guidance and before
    /// the scratchpad — where every composition section has always landed
    /// (stable runtime-context plane).
    Context,
    /// High-churn per-request context, after the git status (transient
    /// plane).
    Transient,
}

impl Rung {
    /// The plane this rung renders on.
    pub const fn plane(self) -> PromptPlane {
        match self {
            Rung::Intro
            | Rung::System
            | Rung::Task
            | Rung::Actions
            | Rung::ToolDiscovery
            | Rung::Style
            | Rung::Efficiency => PromptPlane::Base,
            Rung::Memory | Rung::Context => PromptPlane::Stable,
            Rung::Transient => PromptPlane::Transient,
        }
    }

    /// The rank this rung holds inside its plane — the same numbers the
    /// engine's own table uses, so the two interleave by rank alone.
    pub const fn rank(self) -> u16 {
        match self {
            Rung::Intro => 10,
            Rung::System => 20,
            Rung::Task => 30,
            Rung::Actions => 40,
            Rung::ToolDiscovery => 50,
            Rung::Style => 60,
            Rung::Efficiency => 70,
            Rung::Memory => 40,
            Rung::Context => 75,
            Rung::Transient => 30,
        }
    }

    fn plane_index(self) -> u8 {
        match self.plane() {
            PromptPlane::Base => 0,
            PromptPlane::Stable => 1,
            PromptPlane::Transient => 2,
        }
    }
}

/// One plugin-contributed prompt section.
///
/// `order` is the position inside the rung — the composition's ascending
/// `order` number — and ties break by name, so the same set of sections
/// always renders in the same sequence whatever order they registered in.
#[derive(Debug, Clone)]
pub struct PluginPromptSection {
    pub name: String,
    pub rung: Rung,
    /// Position inside the rung (a JSON number from the Node seat; ties
    /// break by name).
    pub order: f64,
    pub text: String,
}

impl PluginPromptSection {
    /// A section at `rung` with the default order, for providers that
    /// contribute one section per rung.
    pub fn new(name: impl Into<String>, rung: Rung, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rung,
            order: 0.0,
            text: text.into(),
        }
    }

    pub fn with_order(mut self, order: f64) -> Self {
        self.order = order;
        self
    }
}

// `order` is an f64: compare and hash its bits so the section can key a
// cache (`query::session_prompt`) without NaN making equality lie.
impl PartialEq for PluginPromptSection {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.rung == other.rung
            && self.order.to_bits() == other.order.to_bits()
            && self.text == other.text
    }
}

impl Eq for PluginPromptSection {}

impl std::hash::Hash for PluginPromptSection {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.rung.hash(state);
        self.order.to_bits().hash(state);
        self.text.hash(state);
    }
}

/// Sort sections into render order: plane, then rank, then the position
/// inside the rung, then name. Stable, so two sections that tie on all four
/// keep the order they arrived in (provider order on the seat).
pub fn sort_sections(sections: &mut [PluginPromptSection]) {
    sections.sort_by(|a, b| {
        a.rung
            .plane_index()
            .cmp(&b.rung.plane_index())
            .then_with(|| a.rung.rank().cmp(&b.rung.rank()))
            .then_with(|| {
                a.order
                    .partial_cmp(&b.order)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Who a turn's prompt is for.
///
/// The model and the session are the floor; the tool lists are what the turn
/// actually puts in front of the model (the executor's projection, not the
/// engine's whole registry), so a section about a tool can stay out of a
/// turn that does not offer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSubject {
    /// Model identifier the request goes out under (e.g. `"claude-opus-5"`).
    pub model: String,
    /// The session this turn belongs to; `None` for a one-off request.
    pub session_id: Option<String>,
    /// Working directory the turn runs in; `""` when the caller has none.
    ///
    /// A section built out of files on disk needs it: which project's
    /// documents to read, and which project's settings decide whether to read
    /// them at all.
    pub cwd: String,
    /// Whether this turn runs under the coordinator contract, whose prompt is
    /// one opaque base section and whose worker guidance differs.
    pub coordinator: bool,
    /// Tools the model sees eagerly this turn.
    pub tool_names: Vec<String>,
    /// Tools the model can discover through tool search this turn.
    pub deferred_tool_names: Vec<String>,
}

impl PromptSubject {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            session_id: None,
            cwd: String::new(),
            coordinator: false,
            tool_names: Vec::new(),
            deferred_tool_names: Vec::new(),
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// The workspace this turn speaks for: its directory and whether it runs
    /// the coordinator contract.
    pub fn with_workspace(mut self, cwd: impl Into<String>, coordinator: bool) -> Self {
        self.cwd = cwd.into();
        self.coordinator = coordinator;
        self
    }

    pub fn with_tools(mut self, tool_names: Vec<String>, deferred_tool_names: Vec<String>) -> Self {
        self.tool_names = tool_names;
        self.deferred_tool_names = deferred_tool_names;
        self
    }

    /// Whether the turn offers `name`, eagerly or through tool search.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_names.iter().any(|tool| tool == name)
            || self.deferred_tool_names.iter().any(|tool| tool == name)
    }
}

/// A file whose bytes a prompt section put in front of the model.
///
/// The model has "seen" such a file without ever calling `Read`, so the turn
/// seeds it into the read-state cache before the first tool runs — otherwise
/// `Edit` and `Write` refuse to touch a file the prompt just quoted at it.
/// `content` is the raw bytes on disk, not the rendered excerpt, because that
/// is what a later change is diffed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedPromptFile {
    /// Absolute path of the file as it should be cached.
    pub path: std::path::PathBuf,
    /// The raw bytes on disk.
    pub content: String,
    /// Whether the prompt showed less than the whole file (truncated by a
    /// line/byte cap or a runtime budget). A partial view forces the model to
    /// `Read` before it may edit.
    pub is_partial_view: bool,
}

/// One source of prompt sections behind the seat.
///
/// Asked once per turn. An empty answer keeps the provider out of that
/// turn's prompt.
pub trait PromptSectionProvider: Send + Sync {
    fn sections_for(&self, subject: &PromptSubject) -> Vec<PluginPromptSection>;

    /// Files this provider's sections quote verbatim for `cwd`, to seed the
    /// turn's read-state cache with. Default: none, which is right for every
    /// section that is generated text rather than a document.
    ///
    /// Keyed by cwd alone, not by the whole subject: which documents a
    /// project has does not depend on the model or the tool projection, and
    /// the seeding runs at a different point in the turn than the prompt
    /// build.
    fn injected_files(&self, _cwd: &str) -> Vec<InjectedPromptFile> {
        Vec::new()
    }
}

impl<F> PromptSectionProvider for F
where
    F: Fn(&PromptSubject) -> Vec<PluginPromptSection> + Send + Sync,
{
    fn sections_for(&self, subject: &PromptSubject) -> Vec<PluginPromptSection> {
        self(subject)
    }
}

struct ProviderEntry {
    id: String,
    token: u64,
    provider: Arc<dyn PromptSectionProvider>,
}

/// Typed definition for the kernel's `prompt-sections` seat.
///
/// Like the attachment seat, the interface is the seat itself: a plugin
/// registers onto it and the engine's executor reads sections off it, and
/// both faces are needed through the kernel.
pub struct PromptSeatService;

impl Service for PromptSeatService {
    type Interface = PromptSeat;
    const NAME: &'static str = PROMPT_SEAT_SERVICE;
}

/// Provider registry behind the typed `prompt-sections` service.
pub struct PromptSeat {
    entries: RwLock<Vec<ProviderEntry>>,
    next_token: AtomicU64,
}

impl PromptSeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(1),
        })
    }

    /// Register a provider on `ctx`. The registration is an effect of the
    /// context: when `ctx` is disposed (the plugin unloads) the provider
    /// leaves the seat. `id` must be unique within the seat.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        provider: Arc<dyn PromptSectionProvider>,
    ) -> Result<(), KernelError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(KernelError::Other(
                "prompt-sections provider id must be non-empty".into(),
            ));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut entries = self.entries.write().expect("prompt seat poisoned");
            if entries.iter().any(|entry| entry.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: format!("{PROMPT_SEAT_SERVICE}:{id}"),
                });
            }
            entries.push(ProviderEntry {
                id: id.to_string(),
                token,
                provider,
            });
            entries.sort_by(|left, right| left.id.cmp(&right.id));
        }

        let weak = Arc::downgrade(self);
        let id_for_dispose = id.to_string();
        ctx.effect_labeled(&format!("prompt sections({id})"), move || {
            Disposer::new(move || {
                if let Some(seat) = weak.upgrade() {
                    seat.entries
                        .write()
                        .expect("prompt seat poisoned")
                        .retain(|entry| !(entry.id == id_for_dispose && entry.token == token));
                }
            })
        });
        Ok(())
    }

    /// Provider ids currently on the seat, in id order.
    pub fn provider_ids(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("prompt seat poisoned")
            .iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Every section the providers contribute for `subject`, in render
    /// order. Providers are asked outside the seat lock, so one may
    /// register or unregister while another answers.
    pub fn sections_for(&self, subject: &PromptSubject) -> Vec<PluginPromptSection> {
        let providers: Vec<Arc<dyn PromptSectionProvider>> = self
            .entries
            .read()
            .expect("prompt seat poisoned")
            .iter()
            .map(|entry| entry.provider.clone())
            .collect();
        let mut sections: Vec<PluginPromptSection> = providers
            .iter()
            .flat_map(|provider| provider.sections_for(subject))
            .collect();
        sort_sections(&mut sections);
        sections
    }

    /// Every file the providers' sections quote verbatim for `cwd`, in
    /// provider-id order. Providers are asked outside the seat lock, as in
    /// [`Self::sections_for`].
    pub fn injected_files(&self, cwd: &str) -> Vec<InjectedPromptFile> {
        let providers: Vec<Arc<dyn PromptSectionProvider>> = self
            .entries
            .read()
            .expect("prompt seat poisoned")
            .iter()
            .map(|entry| entry.provider.clone())
            .collect();
        providers
            .iter()
            .flat_map(|provider| provider.injected_files(cwd))
            .collect()
    }
}

/// The seat's sections for `subject`, when the kernel scope `ctx` can see a
/// `prompt-sections` seat. Without one — a host with no kernel, a test
/// engine — the answer is empty and the prompt is assembled from the
/// engine's own table alone, which is what such a host got before any of
/// this moved.
pub fn sections_for(ctx: &Context, subject: &PromptSubject) -> Vec<PluginPromptSection> {
    match ctx.get::<PromptSeatService>() {
        Some(seat) => seat.sections_for(subject),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    struct Fixed(Vec<PluginPromptSection>);

    impl PromptSectionProvider for Fixed {
        fn sections_for(&self, _subject: &PromptSubject) -> Vec<PluginPromptSection> {
            self.0.clone()
        }
    }

    fn section(name: &str, rung: Rung, order: f64) -> PluginPromptSection {
        PluginPromptSection::new(name, rung, format!("text:{name}")).with_order(order)
    }

    fn names(sections: &[PluginPromptSection]) -> Vec<&str> {
        sections.iter().map(|s| s.name.as_str()).collect()
    }

    fn subject() -> PromptSubject {
        PromptSubject::new("test-model")
    }

    /// Every base rung ranks where the engine section of the same name
    /// does; the two runtime rungs sit on their own planes.
    #[test]
    fn rungs_map_to_the_engine_tables_planes_and_ranks() {
        assert_eq!(Rung::Style.plane(), PromptPlane::Base);
        assert_eq!(Rung::Style.rank(), 60);
        assert_eq!(Rung::Efficiency.rank(), 70);
        // The two stable rungs, in the order the plane renders them: the
        // loaded documents at the engine table's old `memory` rank, the
        // composition sections after the tool guidance.
        assert_eq!(Rung::Memory.plane(), PromptPlane::Stable);
        assert_eq!(Rung::Memory.rank(), 40);
        assert_eq!(Rung::Context.plane(), PromptPlane::Stable);
        assert_eq!(Rung::Context.rank(), 75);
        assert!(Rung::Memory.rank() < Rung::Context.rank());
        assert_eq!(Rung::Transient.plane(), PromptPlane::Transient);
        let base = [
            Rung::Intro,
            Rung::System,
            Rung::Task,
            Rung::Actions,
            Rung::ToolDiscovery,
            Rung::Style,
            Rung::Efficiency,
        ];
        assert!(base.windows(2).all(|w| w[0].rank() < w[1].rank()));
        assert!(base.iter().all(|rung| rung.plane() == PromptPlane::Base));
    }

    /// Render order is plane, rank, order, name — never registration order
    /// and never provider order when the four keys differ.
    #[test]
    fn sections_come_back_in_render_order_across_providers() {
        let kernel = Kernel::new();
        let seat = PromptSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "zeta",
            Arc::new(Fixed(vec![
                section("z-context", Rung::Context, 5.0),
                section("z-style", Rung::Style, 0.0),
                section("z-transient", Rung::Transient, 0.0),
            ])),
        )
        .unwrap();
        seat.register(
            &ctx,
            "alpha",
            Arc::new(Fixed(vec![
                section("a-context", Rung::Context, 10.0),
                section("a-intro", Rung::Intro, 0.0),
                section("a-style-late", Rung::Style, 1.0),
            ])),
        )
        .unwrap();

        assert_eq!(seat.provider_ids(), vec!["alpha", "zeta"]);
        assert_eq!(
            names(&seat.sections_for(&subject())),
            vec![
                "a-intro",
                "z-style",
                "a-style-late",
                "z-context",
                "a-context",
                "z-transient"
            ]
        );
    }

    /// A provider answers per subject: the model and the offered tools are
    /// what it sees, and an empty answer leaves it out of that turn.
    #[test]
    fn a_provider_answers_per_subject() {
        struct OnlyWithProbe;
        impl PromptSectionProvider for OnlyWithProbe {
            fn sections_for(&self, subject: &PromptSubject) -> Vec<PluginPromptSection> {
                if subject.has_tool("probe") && subject.model.starts_with("gpt-") {
                    vec![section("probe", Rung::Context, 0.0)]
                } else {
                    Vec::new()
                }
            }
        }
        let kernel = Kernel::new();
        let seat = PromptSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(&ctx, "probe", Arc::new(OnlyWithProbe))
            .unwrap();

        assert!(seat.sections_for(&subject()).is_empty());
        let with_probe = PromptSubject::new("gpt-6-astra")
            .with_session_id("sess-1")
            .with_tools(vec![], vec!["probe".into()]);
        assert_eq!(names(&seat.sections_for(&with_probe)), vec!["probe"]);
        let other_model =
            PromptSubject::new("claude-opus-5").with_tools(vec!["probe".into()], vec![]);
        assert!(seat.sections_for(&other_model).is_empty());
    }

    /// Disposing the registering scope takes the provider off the seat.
    #[test]
    fn disposing_the_scope_removes_the_provider() {
        let kernel = Kernel::new();
        let seat = PromptSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "style",
            Arc::new(Fixed(vec![section("style", Rung::Style, 0.0)])),
        )
        .unwrap();
        assert_eq!(names(&seat.sections_for(&subject())), vec!["style"]);

        ctx.dispose();

        assert!(seat.provider_ids().is_empty());
        assert!(seat.sections_for(&subject()).is_empty());
    }

    /// Two registrations under one id is a programming error, not a
    /// last-one-wins — and a refused registration leaves no effect behind.
    #[test]
    fn a_duplicate_provider_id_is_refused() {
        let kernel = Kernel::new();
        let seat = PromptSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(&ctx, "dup", Arc::new(Fixed(Vec::new())))
            .unwrap();
        assert!(seat
            .register(&ctx, "dup", Arc::new(Fixed(Vec::new())))
            .is_err());
        assert_eq!(seat.provider_ids(), vec!["dup"]);
    }

    /// A provider that quotes documents also names them, so the turn can
    /// seed the read-state cache; one that generates text names none.
    #[test]
    fn injected_files_come_from_the_providers_that_have_them() {
        struct Quoting;
        impl PromptSectionProvider for Quoting {
            fn sections_for(&self, _subject: &PromptSubject) -> Vec<PluginPromptSection> {
                vec![section("doc", Rung::Memory, 0.0)]
            }
            fn injected_files(&self, cwd: &str) -> Vec<InjectedPromptFile> {
                vec![InjectedPromptFile {
                    path: std::path::PathBuf::from(cwd).join("MEMORY.md"),
                    content: "- [a](a.md)".into(),
                    is_partial_view: true,
                }]
            }
        }
        let kernel = Kernel::new();
        let seat = PromptSeat::new();
        let ctx = kernel.context().fork("test");
        seat.register(
            &ctx,
            "generated",
            Arc::new(Fixed(vec![section("s", Rung::Style, 0.0)])),
        )
        .unwrap();
        seat.register(&ctx, "quoting", Arc::new(Quoting)).unwrap();

        let files = seat.injected_files("/repo");
        assert_eq!(files.len(), 1, "only the quoting provider names a file");
        assert_eq!(
            files[0].path,
            std::path::PathBuf::from("/repo").join("MEMORY.md")
        );
        assert!(files[0].is_partial_view);

        ctx.dispose();
        assert!(seat.injected_files("/repo").is_empty());
    }

    /// The workspace reaches a provider: a section built out of files on
    /// disk needs the cwd, and the coordinator contract changes what it
    /// says.
    #[test]
    fn a_subject_carries_the_workspace() {
        let bare = subject();
        assert_eq!(bare.cwd, "");
        assert!(!bare.coordinator);
        let scoped = PromptSubject::new("m").with_workspace("/repo", true);
        assert_eq!(scoped.cwd, "/repo");
        assert!(scoped.coordinator);
    }

    /// A scope with no seat above it answers "no sections" rather than
    /// failing the turn.
    #[test]
    fn a_scope_without_a_seat_contributes_nothing() {
        let kernel = Kernel::new();
        assert!(sections_for(kernel.context(), &subject()).is_empty());
    }

    /// The seat is found from a session scope forked under the one that
    /// provides it — both fork kinds, because that is the shape the hosts
    /// hand the executor.
    #[test]
    fn a_session_scope_under_the_providing_scope_sees_the_seat() {
        let kernel = Kernel::new();
        let seat = PromptSeat::new();
        kernel
            .context()
            .provide::<PromptSeatService>(seat.clone())
            .unwrap();
        seat.register(
            kernel.context(),
            "style",
            Arc::new(Fixed(vec![section("seen", Rung::Style, 0.0)])),
        )
        .unwrap();

        let plain = kernel.context().fork("session/abc");
        assert_eq!(names(&sections_for(&plain, &subject())), vec!["seen"]);
        let scoped = kernel.context().fork_scoped("session/def");
        assert_eq!(names(&sections_for(&scoped, &subject())), vec!["seen"]);
    }

    /// Equality and hashing see every field, the rung included: two
    /// sections that differ only in rung are different cache identities.
    #[test]
    fn sections_that_differ_only_in_rung_are_not_equal() {
        use std::collections::HashSet;
        let style = section("s", Rung::Style, 0.0);
        let context = section("s", Rung::Context, 0.0);
        assert_ne!(style, context);
        let set: HashSet<PluginPromptSection> = [style, context].into_iter().collect();
        assert_eq!(set.len(), 2);
    }
}
