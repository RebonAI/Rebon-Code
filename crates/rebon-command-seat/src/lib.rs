//! The `command-registry` seat: every slash command, registered.
//!
//! `rebon-slash-commands` says what a command *is*; this crate is where one
//! is *registered*, by whichever plugin owns it, together with a
//! [`CommandHandler`] saying what running it means. The built-in table is
//! registered by the kernel's `core-commands` plugin; a plugin registers its
//! own the same way, on its own [`Context`], and the registration is an
//! effect — dispose the context and the command is gone, which is what makes
//! a plugin unloadable without a front end knowing it existed.
//!
//! Front ends resolve the leading token of a line with [`CommandSeat::find`]
//! and dispatch on the handler. A [`CommandHandler::Native`] carries a stable
//! id the front end maps to its own implementation; the other three are
//! self-describing enough for any surface to run without knowing the command.
//!
//! No behaviour lives here either. The seat holds specs and handlers; it
//! never runs one.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebon_kernel::{Context, Disposer, KernelError, Service};
pub use rebon_slash_commands::{Category, CommandKind, CommandSpec, Surface, Surfaces};

/// The seat's service name, for the `provides` / `inject` lists a
/// [`rebon_kernel::PluginMeta`] declares.
pub const COMMAND_SEAT_SERVICE: &str = "command-registry";

/// Service definition of the seat. `Interface` is the seat itself: consumers
/// resolve `ctx.require::<CommandSeatService>()` and get the registry.
pub struct CommandSeatService;

impl Service for CommandSeatService {
    type Interface = CommandSeat;
    const NAME: &'static str = COMMAND_SEAT_SERVICE;
}

/// What a typed command line hands a [`CommandHandler::Prompt`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandArgs {
    /// The whole line as typed, trailing whitespace trimmed.
    pub raw: String,
    /// Everything after the command name, trimmed. Empty when there was none.
    pub rest: String,
    /// Where it was typed.
    pub surface: Surface,
}

impl CommandArgs {
    /// Split a typed line into what a handler is given.
    ///
    /// One function rather than one per surface: the terminal, the desktop
    /// app and the headless runners had each written out the same three
    /// lines, and a handler that reads `rest` must not get the arguments
    /// counted one way in a terminal and another in an editor. The spec's own
    /// spellings are what the prefix is stripped by, so a line typed as an
    /// alias hands over the same `rest` as one typed by name.
    pub fn from_line(raw: &str, spec: &CommandSpec, surface: Surface) -> Self {
        let raw = raw.trim_end();
        let rest = rebon_slash_commands::strip_spelling_prefix(raw, spec.spellings())
            .unwrap_or("")
            .trim()
            .to_string();
        Self {
            raw: raw.to_string(),
            rest,
            surface,
        }
    }
}

/// What running a command means.
#[derive(Clone)]
pub enum CommandHandler {
    /// Expand into a prompt for the model: what skills and a plugin's
    /// prompt-shaped commands are.
    ///
    /// `Err` carries the sentence a person should read instead, for a command
    /// that could not be expanded at all -- a plugin that failed, or one that
    /// never answered. It is a `Result` rather than a string a caller has to
    /// recognise: a failure spelled as prompt text is a failure that gets sent
    /// to the model, and every surface would have to sniff for the same prefix
    /// to stop it.
    Prompt(Arc<dyn Fn(&CommandArgs) -> Result<String, String> + Send + Sync>),
    /// Only say something on this surface — a command another surface
    /// implements, or one not implemented yet.
    Explain(Cow<'static, str>),
    /// Open a panel by its dialog id. Until the UI registry exists a front
    /// end matches the id to the panels it has.
    Panel(Cow<'static, str>),
    /// Natively implemented by the front end, which maps the stable id to
    /// its own function. Every built-in is one of these.
    Native(Cow<'static, str>),
}

impl std::fmt::Debug for CommandHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt(_) => f.write_str("Prompt(<fn>)"),
            Self::Explain(text) => f.debug_tuple("Explain").field(text).finish(),
            Self::Panel(id) => f.debug_tuple("Panel").field(id).finish(),
            Self::Native(id) => f.debug_tuple("Native").field(id).finish(),
        }
    }
}

impl CommandHandler {
    /// The id of a [`Self::Native`] handler.
    pub fn native_id(&self) -> Option<&str> {
        match self {
            Self::Native(id) => Some(id),
            _ => None,
        }
    }
}

/// One registration: the spec, how to run it, and who registered it.
#[derive(Clone, Debug)]
pub struct RegisteredCommand {
    pub spec: CommandSpec,
    pub handler: CommandHandler,
    /// The label of the [`Context`] that registered it — the plugin's name,
    /// or `plugin/fork` for a fork — so `/help` can say where a command came
    /// from and a failure can name its owner.
    pub owner: String,
}

struct Entry {
    token: u64,
    command: RegisteredCommand,
}

/// The registry behind the `command-registry` service.
pub struct CommandSeat {
    entries: RwLock<Vec<Entry>>,
    next_token: AtomicU64,
}

impl Default for CommandSeat {
    fn default() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(0),
        }
    }
}

impl CommandSeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a command on `ctx`. The registration is an effect of the
    /// context: disposing it unregisters the command.
    ///
    /// Refused with [`KernelError::DuplicateProvider`] when any spelling of
    /// `spec` — its name or an alias — already names a registered command.
    /// Two commands answering to one token would mean whichever registered
    /// first wins, silently, and that is the drift the seat exists to end.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Context,
        spec: CommandSpec,
        handler: CommandHandler,
    ) -> Result<(), KernelError> {
        let name = spec.name.trim().to_string();
        if name.is_empty() {
            return Err(KernelError::Other(
                "command-registry: a command needs a name".to_string(),
            ));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let owner = ctx.label().to_string();
        {
            let mut entries = self.entries.write().unwrap();
            let clash = entries.iter().find_map(|entry| {
                spec.spellings()
                    .find(|spelling| entry.command.spec.matches(spelling))
                    .map(|spelling| (spelling.to_string(), entry.command.spec.name.to_string()))
            });
            if let Some((spelling, taken_by)) = clash {
                return Err(KernelError::DuplicateProvider {
                    plugin: owner,
                    service: format!("{}:/{spelling} (already /{taken_by})", Self::NAME),
                });
            }
            entries.push(Entry {
                token,
                command: RegisteredCommand {
                    spec,
                    handler,
                    owner,
                },
            });
        }
        let weak = Arc::downgrade(self);
        ctx.effect_labeled(&format!("command(/{name})"), || {
            Disposer::new(move || {
                if let Some(seat) = weak.upgrade() {
                    seat.entries
                        .write()
                        .unwrap()
                        .retain(|entry| entry.token != token);
                }
            })
        });
        Ok(())
    }

    /// The seat's service name.
    pub const NAME: &'static str = CommandSeatService::NAME;

    /// The command a typed token names, by name or alias. Case-insensitive,
    /// like [`CommandSpec::matches`].
    pub fn find(&self, token: &str) -> Option<RegisteredCommand> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .find(|entry| entry.command.spec.matches(token))
            .map(|entry| entry.command.clone())
    }

    /// The commands one surface offers, in registration order.
    pub fn for_surface(&self, surface: Surface) -> Vec<RegisteredCommand> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .filter(|entry| entry.command.spec.available_on(surface))
            .map(|entry| entry.command.clone())
            .collect()
    }

    /// Every registered command, in registration order.
    pub fn all(&self) -> Vec<RegisteredCommand> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .map(|entry| entry.command.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The seat the process kernel provides, if one has booted.
///
/// `None` before the assembly layer boots a kernel, and while the plugin that
/// provides the seat is being reloaded. Both mean the same thing to a caller:
/// nothing is registered right now, so a typed line is prompt text.
pub fn process_command_seat() -> Option<Arc<CommandSeat>> {
    rebon_kernel::process_kernel()?
        .context()
        .get::<CommandSeatService>()
}

/// What one typed line turns out to be.
///
/// For a front end that runs commands but keeps no table of its own: it asks
/// the seat what a line means and acts on the answer. A front end that owns a
/// table of native implementations and its own way of leaving the event loop
/// dispatches on [`CommandHandler`] directly instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypedLine {
    /// No leading slash, or a name nothing has registered. Ordinary prompt
    /// text, sent as typed — which is what leaves a user skill's `/name` for
    /// the engine to resolve.
    NotACommand,
    /// The front end's own implementation, by stable id, with the arguments
    /// the line carried. A surface with no implementation for the id sends
    /// the line as typed, which is what it did before it asked.
    Native { id: String, args: CommandArgs },
    /// What the command expanded to: prompt text for the model.
    Expanded(String),
    /// A sentence for the person, and no model turn. An [`CommandHandler::Explain`]
    /// handler, or a [`CommandHandler::Prompt`] one that could not answer —
    /// a failure spelled as prompt text is a failure that reaches the model.
    Say(String),
    /// A panel, by its `ui-registry` dialog id.
    Panel(String),
}

/// Resolve a typed line against one seat.
///
/// Blocking: a plugin's [`CommandHandler::Prompt`] round-trips to the plugin
/// host process, so an async caller runs this on a blocking thread.
pub fn resolve_typed_line(seat: &CommandSeat, text: &str, surface: Surface) -> TypedLine {
    let raw = text.trim_end();
    let Some(token) = rebon_slash_commands::leading_command_token(raw) else {
        return TypedLine::NotACommand;
    };
    let Some(command) = seat.find(token) else {
        return TypedLine::NotACommand;
    };
    let args = CommandArgs::from_line(raw, &command.spec, surface);
    match command.handler {
        CommandHandler::Native(id) => TypedLine::Native {
            id: id.into_owned(),
            args,
        },
        CommandHandler::Explain(text) => TypedLine::Say(text.into_owned()),
        CommandHandler::Panel(id) => TypedLine::Panel(id.into_owned()),
        CommandHandler::Prompt(expand) => match expand(&args) {
            Ok(expanded) => TypedLine::Expanded(expanded),
            Err(failure) => TypedLine::Say(failure),
        },
    }
}

/// Resolve a typed line against the process kernel's seat.
///
/// [`TypedLine::NotACommand`] when no kernel has booted, which is the answer
/// every surface gave before it asked at all.
pub fn resolve_typed_line_on_process_seat(text: &str, surface: Surface) -> TypedLine {
    match process_command_seat() {
        Some(seat) => resolve_typed_line(&seat, text, surface),
        None => TypedLine::NotACommand,
    }
}

/// A [`rebon_slash_commands::CatalogSource`] over one seat.
///
/// What a host installs so `rebon_slash_commands::{all, find}` read the seat.
/// The seat is looked up on every call rather than captured, so a reload of
/// the plugin that provides it is seen at once.
pub struct SeatCatalogSource<F>(pub F);

impl<F> rebon_slash_commands::CatalogSource for SeatCatalogSource<F>
where
    F: Fn() -> Option<Arc<CommandSeat>> + Send + Sync,
{
    fn all(&self) -> Vec<CommandSpec> {
        (self.0)()
            .map(|seat| seat.all().into_iter().map(|c| c.spec).collect())
            .unwrap_or_default()
    }

    fn find(&self, token: &str) -> Option<CommandSpec> {
        (self.0)()?.find(token).map(|c| c.spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    fn explain(spec: CommandSpec) -> (CommandSpec, CommandHandler) {
        (spec, CommandHandler::Explain(Cow::Borrowed("explained")))
    }

    #[test]
    fn registered_commands_are_found_by_name_and_alias() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("plugin-a");
        let seat = CommandSeat::new();
        let (spec, handler) = explain(CommandSpec::new("exit", "Exit").aliases(["quit"]));
        seat.register(&ctx, spec, handler).unwrap();

        let by_name = seat.find("exit").expect("by name");
        assert_eq!(by_name.spec.name, "exit");
        assert_eq!(by_name.owner, "plugin-a");
        assert!(matches!(by_name.handler, CommandHandler::Explain(ref t) if t == "explained"));
        assert_eq!(
            seat.find("QUIT").map(|c| c.spec.name),
            Some(Cow::Borrowed("exit"))
        );
        assert!(seat.find("exi").is_none());
        assert_eq!(seat.len(), 1);
    }

    #[test]
    fn for_surface_filters_and_all_keeps_registration_order() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("plugin-a");
        let seat = CommandSeat::new();
        for (name, surfaces) in [
            ("help", Surfaces::LOCAL),
            ("vim", Surfaces::TUI_ONLY),
            ("shortcuts", Surfaces::DESKTOP_ONLY),
            ("status", Surfaces::ALL),
        ] {
            let (spec, handler) = explain(CommandSpec::new(name, name).surfaces(surfaces));
            seat.register(&ctx, spec, handler).unwrap();
        }
        let names = |commands: Vec<RegisteredCommand>| {
            commands
                .into_iter()
                .map(|c| c.spec.name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(seat.all()), ["help", "vim", "shortcuts", "status"]);
        assert_eq!(
            names(seat.for_surface(Surface::Tui)),
            ["help", "vim", "status"]
        );
        assert_eq!(
            names(seat.for_surface(Surface::Desktop)),
            ["help", "shortcuts", "status"]
        );
        assert_eq!(names(seat.for_surface(Surface::Acp)), ["status"]);
    }

    #[test]
    fn disposing_the_registering_context_unregisters() {
        let kernel = Kernel::new();
        let seat = CommandSeat::new();
        let core = kernel.context().fork("core");
        let (spec, handler) = explain(CommandSpec::new("help", "Help"));
        seat.register(&core, spec, handler).unwrap();

        let plugin = kernel.context().fork("plugin-b");
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        seat.register(
            &plugin,
            CommandSpec::new("hello", "Say hello").hint("<name>"),
            CommandHandler::Prompt(Arc::new(move |args: &CommandArgs| {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(format!("Say hello to {}", args.rest))
            })),
        )
        .unwrap();
        assert!(plugin
            .registration_labels()
            .iter()
            .any(|label| label == "command(/hello)"));

        let hello = seat.find("hello").expect("registered");
        let CommandHandler::Prompt(expand) = hello.handler else {
            panic!("a prompt handler");
        };
        let args = CommandArgs {
            raw: "/hello world".into(),
            rest: "world".into(),
            surface: Surface::Tui,
        };
        assert_eq!(expand(&args), Ok("Say hello to world".to_string()));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        plugin.dispose();
        assert!(seat.find("hello").is_none(), "gone with its context");
        assert!(seat.find("help").is_some(), "the other owner's stays");
        assert_eq!(seat.len(), 1);
    }

    #[test]
    fn a_name_or_alias_already_taken_is_refused() {
        let kernel = Kernel::new();
        let seat = CommandSeat::new();
        let core = kernel.context().fork("core");
        let (spec, handler) = explain(CommandSpec::new("tasks", "Tasks").aliases(["bg"]));
        seat.register(&core, spec, handler).unwrap();

        let plugin = kernel.context().fork("plugin-c");
        // Same name.
        let (spec, handler) = explain(CommandSpec::new("tasks", "Mine"));
        let err = seat.register(&plugin, spec, handler).unwrap_err();
        assert!(
            matches!(err, KernelError::DuplicateProvider { ref plugin, ref service }
                if plugin == "plugin-c" && service.contains("/tasks")),
            "{err}"
        );
        // An alias that is someone's name, and a name that is someone's alias.
        let (spec, handler) = explain(CommandSpec::new("jobs", "Jobs").aliases(["Tasks"]));
        assert!(matches!(
            seat.register(&plugin, spec, handler),
            Err(KernelError::DuplicateProvider { .. })
        ));
        let (spec, handler) = explain(CommandSpec::new("bg", "Background"));
        assert!(matches!(
            seat.register(&plugin, spec, handler),
            Err(KernelError::DuplicateProvider { .. })
        ));
        assert_eq!(
            seat.len(),
            1,
            "a refused registration leaves nothing behind"
        );
        assert!(
            plugin.registration_labels().is_empty(),
            "and no effect on the refused context"
        );

        // Freed by disposal, the name can be taken again.
        core.dispose();
        let (spec, handler) = explain(CommandSpec::new("tasks", "Mine"));
        seat.register(&plugin, spec, handler).unwrap();
        assert_eq!(seat.find("tasks").map(|c| c.owner), Some("plugin-c".into()));
    }

    /// Every handler shape a headless surface can meet, plus the two ways a
    /// line is not a command at all.
    ///
    /// `/fail` is the one worth spelling out: a prompt handler that could not
    /// answer produces a sentence for the person, never prompt text. Sending
    /// the failure to the model is how a plugin outage used to read as a
    /// question about a plugin outage.
    #[test]
    fn a_typed_line_resolves_to_what_the_handler_says() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("plugin-a");
        let seat = CommandSeat::new();
        seat.register(
            &ctx,
            CommandSpec::new("hello", "Say hello").aliases(["hi"]),
            CommandHandler::Prompt(Arc::new(|args: &CommandArgs| {
                Ok(format!("Say hello to {} on {:?}", args.rest, args.surface))
            })),
        )
        .unwrap();
        seat.register(
            &ctx,
            CommandSpec::new("fail", "Never answers"),
            CommandHandler::Prompt(Arc::new(|_: &CommandArgs| {
                Err("the plugin did not answer".to_string())
            })),
        )
        .unwrap();
        let (spec, handler) = explain(CommandSpec::new("elsewhere", "Elsewhere"));
        seat.register(&ctx, spec, handler).unwrap();
        seat.register(
            &ctx,
            CommandSpec::new("panel", "A panel"),
            CommandHandler::Panel(Cow::Borrowed("some-dialog")),
        )
        .unwrap();
        seat.register(
            &ctx,
            CommandSpec::new("vim", "Toggle vim mode"),
            CommandHandler::Native(Cow::Borrowed("vim")),
        )
        .unwrap();

        let resolve = |text: &str| resolve_typed_line(&seat, text, Surface::Acp);
        assert_eq!(
            resolve("/hello world  "),
            TypedLine::Expanded("Say hello to world on Acp".into())
        );
        // The alias hands over the same `rest` the name does.
        assert_eq!(
            resolve("/hi world"),
            TypedLine::Expanded("Say hello to world on Acp".into())
        );
        assert_eq!(
            resolve("/fail"),
            TypedLine::Say("the plugin did not answer".into())
        );
        assert_eq!(resolve("/elsewhere"), TypedLine::Say("explained".into()));
        assert_eq!(resolve("/panel"), TypedLine::Panel("some-dialog".into()));
        assert_eq!(
            resolve("/vim on"),
            TypedLine::Native {
                id: "vim".into(),
                args: CommandArgs {
                    raw: "/vim on".into(),
                    rest: "on".into(),
                    surface: Surface::Acp,
                },
            }
        );
        assert_eq!(resolve("just talking"), TypedLine::NotACommand);
        assert_eq!(resolve("/nothing-registers-this"), TypedLine::NotACommand);
    }

    #[test]
    fn the_seat_is_a_service_and_a_catalog_source() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("core-commands");
        let seat = CommandSeat::new();
        ctx.provide::<CommandSeatService>(seat.clone()).unwrap();
        let (spec, handler) = explain(CommandSpec::new("help", "Help"));
        seat.register(&ctx, spec, handler).unwrap();

        let resolved = kernel
            .context()
            .require::<CommandSeatService>()
            .expect("provided on the root layer");
        assert!(Arc::ptr_eq(&resolved, &seat));

        let root = kernel.context().clone();
        let source = SeatCatalogSource(move || root.get::<CommandSeatService>());
        use rebon_slash_commands::CatalogSource;
        assert_eq!(source.all().len(), 1);
        assert_eq!(
            source.find("HELP").map(|s| s.name),
            Some(Cow::Borrowed("help"))
        );

        ctx.dispose();
        assert!(source.all().is_empty(), "the source follows the live seat");
        assert!(kernel.context().get::<CommandSeatService>().is_none());
    }
}
