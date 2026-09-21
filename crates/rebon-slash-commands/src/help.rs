//! What the `/help` screen shows: its tabs, and the rows of its command
//! lists.
//!
//! The help screen is a view of the command catalog, so it lives here rather
//! than in a front end. Two things follow from that.
//!
//! The command rows are read from the catalog **when the screen opens**, not
//! from a list a front end sampled at start-up. The terminal seeds its picker
//! before the session exists — before anything has installed the catalog
//! source — so that snapshot is the built-in table, without the commands the
//! agents, tasks, updater and profile plugins register. A help page built from
//! it lists commands that are a subset of the ones that actually run, and stays
//! wrong for the rest of the process even when a plugin is switched off at run
//! time. [`catalog_command_rows`] reads [`crate::for_surface`] on every call
//! instead.
//!
//! User skills are the exception, and the reason [`help_command_rows`] takes
//! its input rather than reading the catalog itself: a skill is not a seat
//! command, so the front end that owns the skill registry passes its own list
//! in for the `custom-commands` tab.
//!
//! Everything here is text and data. Painting a tab strip, wrapping the blurb
//! and clipping a row to the terminal's width belong to the renderer.

use rebon_types::{SlashCommand, SlashCommandCategory};

use crate::{for_surface, Surface};

/// Which of the help screen's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelpTabKey {
    /// The blurb and the shortcut grid.
    General,
    /// Every command the catalog offers on this surface.
    Commands,
    /// The user's own skills.
    Custom,
}

/// One tab of the help screen: what its strip says, and what its list is
/// titled when it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpTab {
    pub key: HelpTabKey,
    /// The label in the tab strip.
    pub title: &'static str,
    /// The heading above the list. `None` on [`HelpTabKey::General`], which
    /// shows no list.
    pub list_title: Option<&'static str>,
    /// What the tab says when its list is empty. `None` on
    /// [`HelpTabKey::General`].
    pub empty_message: Option<&'static str>,
}

/// The help screen's tabs, in the order the strip paints them.
///
/// A fixed table: the tab list used to be computed from a build flag that
/// gated a fourth internal-only tab, and every caller passed the flag as
/// `false` because no command in this binary is internal-only.
pub const HELP_TABS: [HelpTab; 3] = [
    HelpTab {
        key: HelpTabKey::General,
        title: "general",
        list_title: None,
        empty_message: None,
    },
    HelpTab {
        key: HelpTabKey::Commands,
        title: "commands",
        list_title: Some("Browse default commands:"),
        empty_message: Some("No commands found"),
    },
    HelpTab {
        key: HelpTabKey::Custom,
        // The key and the title differ on purpose: the strip has room for the
        // longer word, the code reads better with the shorter one.
        title: "custom-commands",
        list_title: Some("Browse custom commands:"),
        empty_message: Some("No custom commands found"),
    },
];

/// The one-paragraph introduction on the `general` tab.
pub const GENERAL_BLURB: &str =
    "Rebon understands your codebase, makes edits with your permission, \
     and executes commands — right from your terminal.";

/// The heading above the shortcut grid on the `general` tab.
pub const SHORTCUTS_HEADER: &str = "Shortcuts";

/// The title bar of the help screen, with the running build's version.
pub fn version_bar_title(version: &str) -> String {
    format!("Rebon v{version}")
}

/// The footer that names the key which closes the screen.
pub fn dismiss_footer(shortcut: &str) -> String {
    format!("{shortcut} to cancel")
}

/// Which list a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpBucket {
    /// Commands the catalog offers: [`HelpTabKey::Commands`].
    Default,
    /// The user's skills: [`HelpTabKey::Custom`].
    Custom,
}

impl HelpBucket {
    /// The bucket a command belongs to.
    ///
    /// A skill is the user's own; everything else came from the catalog,
    /// whether a built-in registered it or a plugin did.
    pub fn of(command: &SlashCommand) -> Self {
        match command.category {
            Some(SlashCommandCategory::Skill) => Self::Custom,
            _ => Self::Default,
        }
    }
}

/// One line of a help list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpCommandRow {
    /// `/name` plus the argument hint when there is one.
    pub label: String,
    /// The description, with the other spellings appended when the command
    /// answers to more than one.
    pub description: String,
}

/// The rows one list paints, from the commands the caller knows about.
///
/// Sorted by name and deduplicated, because the caller's list is a union of
/// sources that can name the same command twice.
pub fn help_command_rows(commands: &[SlashCommand], bucket: HelpBucket) -> Vec<HelpCommandRow> {
    let mut selected: Vec<&SlashCommand> = commands
        .iter()
        .filter(|command| HelpBucket::of(command) == bucket)
        .collect();
    selected.sort_by(|a, b| a.name.cmp(&b.name));
    selected.dedup_by(|a, b| a.name == b.name);
    selected.into_iter().map(row_for).collect()
}

/// The rows the `commands` tab paints: the command-registry seat, read now.
///
/// See the module docs for why this reads the catalog instead of taking a
/// list the caller sampled earlier.
pub fn catalog_command_rows(surface: Surface) -> Vec<HelpCommandRow> {
    help_command_rows(&for_surface(surface), HelpBucket::Default)
}

fn row_for(command: &SlashCommand) -> HelpCommandRow {
    let hint = command
        .input
        .as_ref()
        .and_then(|input| input.hint.as_deref())
        .filter(|hint| !hint.is_empty());
    let label = match hint {
        Some(hint) => format!("/{} {hint}", command.name),
        None => format!("/{}", command.name),
    };
    let mut description = command.description.clone();
    if !command.aliases.is_empty() {
        let aliases = command
            .aliases
            .iter()
            .map(|alias| format!("/{alias}"))
            .collect::<Vec<_>>()
            .join(", ");
        description.push_str(&format!(" (alias: {aliases})"));
    }
    HelpCommandRow { label, description }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::SlashCommandInput;

    fn command(name: &str, category: SlashCommandCategory) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: format!("{name} description"),
            input: None,
            category: Some(category),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn the_tab_strip_has_three_tabs_in_a_fixed_order() {
        let keys: Vec<HelpTabKey> = HELP_TABS.iter().map(|tab| tab.key).collect();
        assert_eq!(
            keys,
            vec![
                HelpTabKey::General,
                HelpTabKey::Commands,
                HelpTabKey::Custom
            ]
        );
        assert_eq!(HELP_TABS[0].list_title, None);
        assert_eq!(HELP_TABS[1].list_title, Some("Browse default commands:"));
        assert_eq!(HELP_TABS[2].empty_message, Some("No custom commands found"));
    }

    #[test]
    fn skills_go_to_the_custom_bucket_and_everything_else_to_the_default_one() {
        assert_eq!(
            HelpBucket::of(&command("commit", SlashCommandCategory::Skill)),
            HelpBucket::Custom
        );
        assert_eq!(
            HelpBucket::of(&command("clear", SlashCommandCategory::Command)),
            HelpBucket::Default
        );
        assert_eq!(
            HelpBucket::of(&command("agents", SlashCommandCategory::Agent)),
            HelpBucket::Default
        );
        let mut uncategorised = command("mystery", SlashCommandCategory::Command);
        uncategorised.category = None;
        assert_eq!(HelpBucket::of(&uncategorised), HelpBucket::Default);
    }

    #[test]
    fn rows_are_sorted_deduplicated_and_split_by_bucket() {
        let commands = vec![
            command("status", SlashCommandCategory::Command),
            command("commit", SlashCommandCategory::Skill),
            command("clear", SlashCommandCategory::Command),
            command("status", SlashCommandCategory::Command),
        ];

        let default = help_command_rows(&commands, HelpBucket::Default);
        assert_eq!(
            default
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["/clear", "/status"]
        );

        let custom = help_command_rows(&commands, HelpBucket::Custom);
        assert_eq!(
            custom
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["/commit"]
        );
    }

    #[test]
    fn a_hint_joins_the_label_and_aliases_join_the_description() {
        let mut spec = command("model", SlashCommandCategory::Command);
        spec.input = Some(SlashCommandInput {
            hint: Some("<name>".to_string()),
        });
        spec.aliases = vec!["m".to_string(), "models".to_string()];

        let rows = help_command_rows(&[spec], HelpBucket::Default);
        assert_eq!(rows[0].label, "/model <name>");
        assert_eq!(
            rows[0].description,
            "model description (alias: /m, /models)"
        );
    }

    #[test]
    fn an_empty_hint_does_not_widen_the_label() {
        let mut spec = command("clear", SlashCommandCategory::Command);
        spec.input = Some(SlashCommandInput {
            hint: Some(String::new()),
        });
        assert_eq!(
            help_command_rows(&[spec], HelpBucket::Default)[0].label,
            "/clear"
        );
    }

    #[test]
    fn the_title_bar_and_the_dismiss_footer_read_as_sentences() {
        assert_eq!(version_bar_title("1.2.3"), "Rebon v1.2.3");
        assert_eq!(dismiss_footer("Esc"), "Esc to cancel");
    }
}
