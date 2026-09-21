//! What slash commands exist, in the one place every front end can reach.
//!
//! The catalog used to be duplicated: each front end carried its own
//! `parse_x_command` functions and its own registration table. Two lists
//! drift, and these had already drifted in both directions — commands one
//! front end parses but never lists, commands another offers that the first
//! spells differently — and each drift is a command that works in one place
//! and silently becomes prompt text in the other.
//!
//! # What this crate says, and what it refuses to say
//!
//! It says what a command **is**: its name, what else it answers to, what
//! arguments it takes, and where it works. It says nothing about what happens
//! when you run one — `/settings` opens a dialog in one front end and a
//! window in another, and neither is more correct. A front end matches on
//! [`CommandSpec::name`] and decides for itself. The [`formatters`] module
//! only projects caller-sampled DTOs to read-only command text; it never
//! samples runtime state or dispatches a command.
//!
//! That line is the whole design. The moment this crate knows that `/new`
//! clears a specific view, it stops being reachable from the other front end,
//! and the drift starts again from the other end.
//!
//! # Where the catalog lives now
//!
//! The command seat registers the [`builtin_command_table`], and plugins
//! register theirs on the same seat. This crate cannot depend on the kernel
//! without a cycle, so the readers' entry points ([`all`], [`find`],
//! [`for_surface`], [`parse`]) go through a [`CatalogSource`] the seat's host
//! installs at boot with [`install_catalog_source`]. Before that happens
//! readers fall back to the built-in table; once installed, the live source is
//! authoritative.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

pub mod builtin;
pub mod formatters;
pub mod help;
pub use builtin::builtin_command_table;

/// One command's identity.
///
/// Deliberately carries no behaviour. See the module docs for why.
///
/// Every string is a [`Cow`] so the built-in table can be written with
/// literals and a plugin can build one at run time from whatever it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// Canonical name, without the leading slash.
    pub name: Cow<'static, str>,
    /// Other spellings that resolve to this command.
    pub aliases: Vec<Cow<'static, str>>,
    /// Chinese words that *find* this command in a `/` menu.
    ///
    /// Deliberately not part of [`Self::matches`]: `/新建` is not a command, it
    /// is a way to reach `/new`, which a menu then inserts by its English name.
    /// Parsing them would also let a built-in shadow a user skill that happens
    /// to carry a Chinese name.
    pub zh_aliases: Vec<Cow<'static, str>>,
    /// Argument hint shown in a menu (`[on|off]`, `<prompt>`, …).
    pub hint: Option<Cow<'static, str>>,
    /// One line, in the imperative. Shown in menus and `/help`.
    pub description: Cow<'static, str>,
    pub category: Category,
    /// Where this command actually does something.
    pub surfaces: Surfaces,
    /// What shape the command has, for a front end deciding how to present
    /// it. Advisory: the seat's handler is what actually runs.
    pub kind: CommandKind,
}

impl CommandSpec {
    /// A command with a name and a description; everything else is set with
    /// the builder methods. Local to both front ends by default, a plain
    /// command, natively implemented.
    pub fn new(
        name: impl Into<Cow<'static, str>>,
        description: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            zh_aliases: Vec::new(),
            hint: None,
            description: description.into(),
            category: Category::Command,
            surfaces: Surfaces::LOCAL,
            kind: CommandKind::Native,
        }
    }

    pub fn aliases<I, S>(mut self, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Cow<'static, str>>,
    {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }

    pub fn zh_aliases<I, S>(mut self, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Cow<'static, str>>,
    {
        self.zh_aliases = aliases.into_iter().map(Into::into).collect();
        self
    }

    pub fn hint(mut self, hint: impl Into<Cow<'static, str>>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn category(mut self, category: Category) -> Self {
        self.category = category;
        self
    }

    pub fn surfaces(mut self, surfaces: Surfaces) -> Self {
        self.surfaces = surfaces;
        self
    }

    pub fn kind(mut self, kind: CommandKind) -> Self {
        self.kind = kind;
        self
    }

    /// Whether a typed token names this command.
    ///
    /// Exact apart from ASCII case: every command name in the catalog is
    /// lowercase ASCII, so `/Help` is a shift key held a moment too long, not a
    /// different command. The TUI used to compare exactly and the app used to
    /// lowercase first, which meant `/Compact` worked on the desktop and became
    /// prompt text in the terminal.
    ///
    /// Only the catalog lookup is lenient. A name that matches nothing is
    /// returned verbatim so a front end can look it up in its own skill
    /// registry, where case may well matter.
    pub fn matches(&self, token: &str) -> bool {
        self.name.eq_ignore_ascii_case(token)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(token))
    }

    /// Whether this command does anything on the given surface.
    pub fn available_on(&self, surface: Surface) -> bool {
        self.surfaces.contains(surface)
    }

    /// Every spelling that parses as this command: the name, then the aliases.
    pub fn spellings(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_ref()).chain(self.aliases.iter().map(Cow::as_ref))
    }

    /// The wire form, for `session/new`'s `slashCommands` and the `/` menu.
    pub fn to_wire(&self) -> rebon_types::SlashCommand {
        rebon_types::SlashCommand {
            name: self.name.to_string(),
            description: self.description.to_string(),
            input: self
                .hint
                .as_deref()
                .map(|hint| rebon_types::SlashCommandInput {
                    hint: Some(hint.to_string()),
                }),
            category: Some(self.category.to_wire()),
            aliases: self.aliases.iter().map(|a| a.to_string()).collect(),
        }
    }
}

/// Which grouping a command belongs to in a picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// Built-in command (`/help`, `/clear`, `/compact`, …).
    Command,
    /// Agent-related (`/agent`, `/agents`, `/teams`, …).
    Agent,
}

impl Category {
    pub fn to_wire(self) -> rebon_types::SlashCommandCategory {
        match self {
            Self::Command => rebon_types::SlashCommandCategory::Command,
            Self::Agent => rebon_types::SlashCommandCategory::Agent,
        }
    }
}

/// The shape of a command, as a front end sees it.
///
/// Advisory. The command seat's handler decides what actually happens; this
/// is what a menu or a help page groups by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    /// Opens a panel, dialog or overlay (`/settings`, `/tasks`, `/rewind`).
    Panel,
    /// Runs against the session's engine and answers in the transcript;
    /// a mirror forwards it to the session's owner (`/context`, `/compact`).
    Session,
    /// Expands into a model turn (`/review`, `/ultrawork`).
    Prompt,
    /// Only explains itself on this binary's surfaces.
    Explain,
    /// Implemented natively by the front end (`/vim`, `/exit`, `/run`).
    Native,
}

/// A front end that runs commands.
///
/// This is not a statement about who *may* type a command — it is what the
/// catalog knows about where one has an implementation. A command typed on a
/// surface it does not support should be explained, not sent to the model as
/// prompt text, which is what used to happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The terminal UI.
    Tui,
    /// The desktop app.
    Desktop,
    /// The ACP server, driven by an editor over stdio.
    ///
    /// The narrowest of the three, and deliberately so: an editor is told what
    /// the server actually implements, and advertising more than that produces
    /// a command that appears in the editor's picker and does nothing.
    Acp,
    /// The `serve` web page.
    Web,
    /// The mobile app, over the relay.
    Mobile,
    /// Not a front end: the set of commands a mirror forwards to the process
    /// that owns the session, because they read or rewrite the session's
    /// engine state. A purely local UI command (`/vim`, `/help`) is not one.
    SessionControl,
}

/// The set of surfaces a command works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surfaces(u8);

impl Surfaces {
    const TUI: u8 = 1 << 0;
    const DESKTOP: u8 = 1 << 1;
    const ACP: u8 = 1 << 2;
    const WEB_BIT: u8 = 1 << 3;
    const MOBILE_BIT: u8 = 1 << 4;
    const SESSION_CONTROL_BIT: u8 = 1 << 5;

    /// Nothing.
    pub const NONE: Self = Self(0);
    /// Every front end, the ACP server included.
    pub const ALL: Self = Self(Self::TUI | Self::DESKTOP | Self::ACP);
    /// Both local front ends, but not over the wire. The common case: most
    /// commands need a session someone is sitting in front of.
    pub const LOCAL: Self = Self(Self::TUI | Self::DESKTOP);
    /// Terminal only.
    pub const TUI_ONLY: Self = Self(Self::TUI);
    /// Desktop only.
    pub const DESKTOP_ONLY: Self = Self(Self::DESKTOP);
    /// The ACP server only.
    pub const ACP_ONLY: Self = Self(Self::ACP);
    /// The `serve` page. Combine with [`Self::with`].
    pub const WEB: Self = Self(Self::WEB_BIT);
    /// The mobile app. Combine with [`Self::with`].
    pub const MOBILE: Self = Self(Self::MOBILE_BIT);
    /// Forwarded to the session's owner by a mirror. Combine with
    /// [`Self::with`].
    pub const SESSION_CONTROL: Self = Self(Self::SESSION_CONTROL_BIT);

    /// Union.
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, surface: Surface) -> bool {
        let bit = match surface {
            Surface::Tui => Self::TUI,
            Surface::Desktop => Self::DESKTOP,
            Surface::Acp => Self::ACP,
            Surface::Web => Self::WEB_BIT,
            Surface::Mobile => Self::MOBILE_BIT,
            Surface::SessionControl => Self::SESSION_CONTROL_BIT,
        };
        self.0 & bit != 0
    }
}

/// What a line of composer text turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    /// Ordinary prompt text — no leading slash, or a bare `/`.
    NotACommand,
    /// Starts with a slash but names nothing in the catalog.
    ///
    /// Not an error by itself: user skills and plugin-registered commands are
    /// resolved by the front end, which checks its own registry before
    /// treating this as a mistake.
    Unknown {
        /// The typed name, without the slash.
        name: &'a str,
        /// Everything after the name, trimmed. Empty when there was none.
        args: &'a str,
    },
    /// A catalog command.
    Command {
        spec: CommandSpec,
        /// Everything after the name, trimmed. Empty when there was none.
        args: &'a str,
    },
}

/// Where the catalog comes from at run time.
///
/// Implemented once, by the host of the kernel's command seat, and installed
/// with [`install_catalog_source`]. This crate cannot reach the seat itself:
/// the seat depends on this crate for [`CommandSpec`], and a dependency the
/// other way would be a cycle.
pub trait CatalogSource: Send + Sync {
    /// Every registered command, in registration (menu) order.
    fn all(&self) -> Vec<CommandSpec>;
    /// The command a typed token names, by name or alias.
    fn find(&self, token: &str) -> Option<CommandSpec>;
}

static CATALOG_SOURCE: OnceLock<Arc<dyn CatalogSource>> = OnceLock::new();
static WARNED_NO_SOURCE: AtomicBool = AtomicBool::new(false);

/// Install the process-wide catalog source. The first installation wins;
/// a later one is refused and `false` is returned, which is fine for a
/// source that resolves the live seat on every call.
pub fn install_catalog_source(source: Arc<dyn CatalogSource>) -> bool {
    CATALOG_SOURCE.set(source).is_ok()
}

/// Whether [`install_catalog_source`] has happened.
pub fn catalog_source_installed() -> bool {
    CATALOG_SOURCE.get().is_some()
}

fn source() -> Option<&'static Arc<dyn CatalogSource>> {
    let source = CATALOG_SOURCE.get();
    if source.is_none() && !WARNED_NO_SOURCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "slash-command catalog read before the kernel's command seat was installed; \
             the catalog is empty until `core-commands` loads"
        );
    }
    source
}

/// Every command, in menu order.
///
/// The order is a product judgement, not an accident: the things people reach
/// for first, then settings surfaces, then model-facing commands. A front end
/// showing a picker for an empty query shows this order. Falls back to the
/// built-in table until a [`CatalogSource`] is installed.
pub fn all() -> Vec<CommandSpec> {
    source()
        .map(|source| source.all())
        .unwrap_or_else(builtin_command_table)
}

/// The command a typed token names, if any.
///
/// Matches the canonical name and its aliases, never the Chinese aliases —
/// those find a command in a menu but are not names for it. See
/// [`CommandSpec::zh_aliases`].
pub fn find(token: &str) -> Option<CommandSpec> {
    match source() {
        Some(source) => source.find(token),
        None => builtin_command_table()
            .into_iter()
            .find(|spec| spec.matches(token)),
    }
}

/// Whether the command a typed token names does anything on `surface`.
///
/// [`for_surface`] asked from the other end, for a front end holding a name
/// rather than building a menu. The four hand-written lists this replaced —
/// what the web page offers, what the desktop relays to a phone, what a
/// mirror forwards to a session's owner, what the ACP server answers — were
/// each a second spelling of one surface bit, and a plugin registering a
/// command for one of those surfaces had to be added in two places.
pub fn available_on(token: &str, surface: Surface) -> bool {
    find(token).is_some_and(|spec| spec.available_on(surface))
}

/// The commands one surface offers, in wire form.
///
/// What a picker is seeded from, and what the ACP server advertises. Both had
/// written this filter out themselves.
pub fn for_surface(surface: Surface) -> Vec<rebon_types::SlashCommand> {
    all()
        .iter()
        .filter(|spec| spec.available_on(surface))
        .map(CommandSpec::to_wire)
        .collect()
}

/// The first token of a slash line, without the slash: what a seat lookup
/// keys on.
///
/// `None` for anything that is not a command line at all — no leading slash
/// or a bare `/`. The token ends at the first whitespace or `:`, the two
/// argument separators the built-in grammars accept (`/tasks shell-7` and
/// `/tasks:shell-7` name the same command). Whether what follows is
/// well-formed is the command's own business.
///
/// Only the first line names the command; the lines after it are argument
/// text. `/ceo <task>`, `/ultraplan <prompt>` and `/goal` take prose that
/// people write across several lines, and a multi-line line used to be
/// refused here outright, which sent the whole thing to the model with the
/// `/ceo` still on it. A pasted diff or path that happens to open with `/`
/// is still safe: its first line names no command, so the catalog lookup
/// that follows this token turns it away.
pub fn leading_command_token(text: &str) -> Option<&str> {
    let line = text.trim().lines().next().unwrap_or("").trim_end();
    let rest = line.strip_prefix('/')?;
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ':')
        .unwrap_or(rest.len());
    let token = &rest[..end];
    (!token.is_empty()).then_some(token)
}

/// Read one line of composer text.
///
/// Leading and trailing whitespace is ignored, which is a behaviour change the
/// TUI needed: it had `text.trim() == "/help"` in one place and
/// `text == "/onboarding"` in another, so a trailing space made one command
/// work and the other silently become prompt text.
///
/// Only the first line is considered. A multi-line paste that happens to start
/// with `/` is prompt text — someone pasting a diff should not have their first
/// line eaten as a command.
pub fn parse(text: &str) -> Parsed<'_> {
    let line = text.trim();
    if line.contains('\n') {
        return Parsed::NotACommand;
    }
    let Some(rest) = line.strip_prefix('/') else {
        return Parsed::NotACommand;
    };
    if rest.is_empty() {
        return Parsed::NotACommand;
    }

    let (name, args) = match rest.find(char::is_whitespace) {
        Some(idx) => (&rest[..idx], rest[idx..].trim()),
        None => (rest, ""),
    };

    match find(name) {
        Some(spec) => Parsed::Command { spec, args },
        None => Parsed::Unknown { name, args },
    }
}

/// Strip a leading `/name` — or any spelling the catalog lists for it — off a
/// composer line, and return what follows.
///
/// Only recognizing the *name* is shared; what comes back is read by each
/// parser's own grammar, because those genuinely differ (`/run` takes the rest
/// verbatim, `/tasks:shell-7` separates with a colon). Recognition is the part
/// that had drifted: most parsers compared one exact literal while the catalog
/// promised case never decides what a command is, so `/Compact` was prompt text
/// and `/statusLine`, spelled out by hand, was not.
///
/// A leading space still means prompt text — `" /help"` is how you talk *about*
/// a command — which is why none of these parsers trim the front. Trailing
/// space is the caller's business; the TUI's composer hands every parser
/// What the terminal says for a command only the desktop app implements.
pub const DESKTOP_ONLY_EXPLANATION: &str = "This command is available in the desktop app.";

/// Where a command lives, for a surface that has no implementation of it.
///
/// Next to [`Surfaces`] rather than in a front end because it answers the same
/// question the surfaces table does — where does this command work — and the
/// answer has to agree with it. Worth the specificity: "not available here"
/// leaves someone stuck, while "Settings > Models" is what they were after.
pub fn unavailable_explanation(name: &str, surface: Surface) -> &'static str {
    if surface != Surface::Desktop {
        return DESKTOP_ONLY_EXPLANATION;
    }
    match name {
        "resume" => "Pick a past session from the sidebar list.",
        "vim" => "The desktop composer has no vim mode.",
        "exit" => "Close the window to quit the desktop app.",
        "fast" => "Toggle the fast tier from the model menu; there is no command for it here.",
        _ => "Only available in the rebon CLI.",
    }
}

/// `text.trim_end()`. The longest spelling wins, so `/hosted` is not `/host`
/// with the argument `ed`.
///
/// Lives here rather than in the terminal because the aliases it consults are
/// the catalog's: a second copy would be a second answer to "is this a
/// command", which is the drift the catalog was built to end.
pub fn strip_command_prefix<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let spec = find(name);
    let aliases = spec
        .as_ref()
        .map(|spec| spec.aliases.as_slice())
        .unwrap_or_default();
    strip_spelling_prefix(
        text,
        std::iter::once(name).chain(aliases.iter().map(Cow::as_ref)),
    )
}

/// [`strip_command_prefix`], for a caller that already holds the spec.
///
/// The same rule, given the spellings directly instead of looking them up:
/// a caller resolving a line against a seat it holds has the spec in hand,
/// and the catalog lookup [`strip_command_prefix`] does would answer from
/// the *process-wide* source — which is a different seat in a test, and
/// nothing at all before a kernel boots. Both then strip nothing, and the
/// arguments of a line typed by alias silently become the empty string.
pub fn strip_spelling_prefix<'a, 'b>(
    text: &'a str,
    spellings: impl Iterator<Item = &'b str>,
) -> Option<&'a str> {
    if text.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = text.strip_prefix('/')?;
    spellings
        .filter(|spelling| {
            // `get` rather than indexing: a line opening with a multi-byte
            // character would split one mid-way and panic.
            rest.get(..spelling.len())
                .is_some_and(|typed| typed.eq_ignore_ascii_case(spelling))
        })
        .max_by_key(|spelling| spelling.len())
        .map(|spelling| &rest[spelling.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the seat: enough of the built-in table to exercise the
    /// parser. The real table belongs to the seat's host, whose tests check its
    /// content.
    struct TestCatalog(Vec<CommandSpec>);

    impl CatalogSource for TestCatalog {
        fn all(&self) -> Vec<CommandSpec> {
            self.0.clone()
        }
        fn find(&self, token: &str) -> Option<CommandSpec> {
            self.0.iter().find(|spec| spec.matches(token)).cloned()
        }
    }

    fn install_test_catalog() {
        let _ = install_catalog_source(Arc::new(TestCatalog(vec![
            CommandSpec::new("help", "Show available commands").zh_aliases(["帮助"]),
            CommandSpec::new("new", "Start a fresh conversation").zh_aliases(["新建", "新会话"]),
            CommandSpec::new("compact", "Compact the conversation now")
                .zh_aliases(["压缩"])
                .hint("[instructions]"),
            CommandSpec::new("agent", "Spawn a background sub-agent")
                .category(Category::Agent)
                .hint("<prompt>"),
            CommandSpec::new("exit", "Exit the application")
                .aliases(["quit"])
                .surfaces(Surfaces::TUI_ONLY),
            CommandSpec::new("hosted", "Detach this session into a background worker")
                .aliases(["host"])
                .surfaces(Surfaces::TUI_ONLY),
            CommandSpec::new("status", "Show session status")
                .surfaces(Surfaces::ALL.with(Surfaces::WEB).with(Surfaces::MOBILE)),
        ])));
    }

    #[test]
    fn plain_text_is_not_a_command() {
        install_test_catalog();
        assert_eq!(parse("hello"), Parsed::NotACommand);
        assert_eq!(parse(""), Parsed::NotACommand);
        assert_eq!(parse("   "), Parsed::NotACommand);
        assert_eq!(parse("/"), Parsed::NotACommand);
        assert_eq!(parse("not /help"), Parsed::NotACommand);
    }

    /// The TUI trimmed in some parsers and not others, so `/help ` worked and
    /// `/onboarding ` became prompt text. One parser, one answer.
    #[test]
    fn surrounding_whitespace_never_decides_whether_a_command_is_one() {
        install_test_catalog();
        for text in ["/help", "/help ", " /help", "  /help  ", "\t/help\n"] {
            match parse(text) {
                Parsed::Command { spec, args } => {
                    assert_eq!(spec.name, "help", "{text:?}");
                    assert_eq!(args, "", "{text:?}");
                }
                other => panic!("{text:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn a_multiline_paste_is_prompt_text_even_when_it_opens_with_a_slash() {
        install_test_catalog();
        assert_eq!(
            parse("/usr/bin/thing\nsecond line"),
            Parsed::NotACommand,
            "a pasted diff or path must not have its first line eaten"
        );
        // A pasted path is tokenized from its first line and then fails the
        // catalog lookup; the tokenizer itself no longer refuses newlines.
        assert_eq!(
            leading_command_token("/usr/bin/thing\nsecond line"),
            Some("usr/bin/thing")
        );
        assert!(find("usr/bin/thing").is_none());
    }

    #[test]
    fn arguments_are_whatever_follows_the_name() {
        install_test_catalog();
        match parse("/agent  write the tests  ") {
            Parsed::Command { spec, args } => {
                assert_eq!(spec.name, "agent");
                assert_eq!(args, "write the tests");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn an_alias_resolves_to_its_command() {
        install_test_catalog();
        match parse("/quit") {
            Parsed::Command { spec, .. } => assert_eq!(spec.name, "exit"),
            other => panic!("parsed as {other:?}"),
        }
    }

    /// `/Compact` used to work on the desktop and become prompt text in the
    /// terminal. Every catalog name is lowercase ASCII, so case cannot be
    /// carrying meaning.
    #[test]
    fn a_held_shift_key_does_not_change_what_a_command_is() {
        install_test_catalog();
        for text in ["/compact", "/Compact", "/COMPACT"] {
            match parse(text) {
                Parsed::Command { spec, .. } => assert_eq!(spec.name, "compact", "{text:?}"),
                other => panic!("{text:?} parsed as {other:?}"),
            }
        }
    }

    /// The leniency stops at the catalog. A name nobody knows comes back
    /// exactly as typed, because the front end will look it up somewhere that
    /// may well care.
    #[test]
    fn an_unknown_name_keeps_the_case_it_was_typed_in() {
        install_test_catalog();
        assert_eq!(
            parse("/MySkill"),
            Parsed::Unknown {
                name: "MySkill",
                args: ""
            }
        );
    }

    /// A name nobody knows is not an error here — user skills and plugin
    /// commands are resolved by the front end against its own registry.
    #[test]
    fn an_unknown_name_carries_through_for_the_front_end_to_resolve() {
        install_test_catalog();
        assert_eq!(
            parse("/simplify src/lib.rs"),
            Parsed::Unknown {
                name: "simplify",
                args: "src/lib.rs"
            }
        );
    }

    #[test]
    fn chinese_aliases_find_a_command_but_do_not_parse_as_one() {
        install_test_catalog();
        let new = find("new").expect("in catalog");
        assert!(
            new.zh_aliases.iter().any(|alias| alias == "新建"),
            "the menu can find it"
        );
        assert!(!new.matches("新建"), "but it is not a second name for it");
        assert_eq!(
            parse("/新建"),
            Parsed::Unknown {
                name: "新建",
                args: ""
            },
            "so a user skill named 新建 is still reachable"
        );
    }

    /// The longest spelling wins, and the aliases come from the catalog.
    #[test]
    fn strip_command_prefix_consults_the_catalog_for_aliases() {
        install_test_catalog();
        assert_eq!(strip_command_prefix("/hosted", "hosted"), Some(""));
        assert_eq!(strip_command_prefix("/host", "hosted"), Some(""));
        assert_eq!(strip_command_prefix("/Quit now", "exit"), Some(" now"));
        assert_eq!(strip_command_prefix(" /help", "help"), None);
        assert_eq!(strip_command_prefix("/新建", "new"), None);
    }

    /// The seat keys on the first token; both argument separators end it.
    #[test]
    fn the_leading_token_ends_at_whitespace_or_colon() {
        assert_eq!(leading_command_token("/tasks shell-7"), Some("tasks"));
        assert_eq!(leading_command_token("/tasks:shell-7"), Some("tasks"));
        assert_eq!(leading_command_token("  /Help  "), Some("Help"));
        assert_eq!(
            leading_command_token("/background-agents"),
            Some("background-agents")
        );
        assert_eq!(leading_command_token("/"), None);
        assert_eq!(leading_command_token("hello"), None);
        assert_eq!(leading_command_token(""), None);
    }

    /// A coordinator task written across several lines is still `/ceo`;
    /// the newline gate that used to refuse it sent the whole prompt to the
    /// model with the command still on the front.
    #[test]
    fn leading_command_token_reads_the_command_off_the_first_line_of_a_multi_line_task() {
        assert_eq!(
            leading_command_token(
                "/ceo 请你直接安排多个 agent 处理这些事\n- 有前置的包别先派\n- 各自按路径提交"
            ),
            Some("ceo")
        );
        assert_eq!(
            leading_command_token("/ultraplan --grill 设计 session-host\n\n约束：不动 RFC-0004"),
            Some("ultraplan")
        );
        assert_eq!(leading_command_token("\n/ceo later"), Some("ceo"));
    }

    #[test]
    fn surface_bits_compose() {
        let spec = find("status").expect("in catalog");
        for surface in [
            Surface::Tui,
            Surface::Desktop,
            Surface::Acp,
            Surface::Web,
            Surface::Mobile,
        ] {
            assert!(spec.available_on(surface), "{surface:?}");
        }
        assert!(!spec.available_on(Surface::SessionControl));
        let control = Surfaces::LOCAL.with(Surfaces::SESSION_CONTROL);
        assert!(control.contains(Surface::SessionControl));
        assert!(control.contains(Surface::Tui));
        assert!(!control.contains(Surface::Acp));
        assert!(!Surfaces::NONE.contains(Surface::Tui));
    }

    #[test]
    fn the_wire_form_carries_hint_and_aliases() {
        install_test_catalog();
        let wire = find("exit").expect("in catalog").to_wire();
        assert_eq!(wire.name, "exit");
        assert_eq!(wire.aliases, ["quit"]);
        assert!(wire.input.is_none());
        let wire = find("compact").expect("in catalog").to_wire();
        assert_eq!(
            wire.input.and_then(|input| input.hint).as_deref(),
            Some("[instructions]")
        );
    }
}
