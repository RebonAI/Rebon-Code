//! `--dangerously-load-development-channels` warning dialog.
//!
//! ## Behaviour
//!
//! * The fixed title + warning paragraphs + dialog color (Error).
//! * The two-option list (`I am using this for local development` /
//!   `Exit`).
//! * The channel-list summary projection (`plugin:foo@market`,
//!   `server:bar`).
//! * The cancel branch (exit with code 0).
//! * The accept branch (continue startup) and the decline branch
//!   (exit with code 1).

use crate::common::{DialogColor, SelectOption};

/// The dialog title.
pub const TITLE: &str = "WARNING: Loading development channels";

/// The first warning paragraph.
pub const WARNING_PARAGRAPH_1: &str = "--dangerously-load-development-channels is for local channel development only. Do not use this option to run channels you have downloaded off the internet.";

/// The second warning paragraph.
pub const WARNING_PARAGRAPH_2: &str = "Please use --channels to run a list of approved channels.";

/// The dialog frame color.
pub const DIALOG_COLOR: DialogColor = DialogColor::Error;

/// A pre-built channel entry: a marketplace plugin or a named server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEntryInput {
    /// `plugin:{name}@{marketplace}`.
    Plugin {
        /// Plugin name.
        name: String,
        /// Marketplace identifier.
        marketplace: String,
    },
    /// `server:{name}`.
    Server {
        /// Server name.
        name: String,
    },
}

/// Option values: `accept` or `exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevChannelsValue {
    /// "I am using this for local development".
    Accept,
    /// "Exit".
    Exit,
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevChannelsAction {
    /// Continue startup with the development channels loaded.
    Accept,
    /// Hard-exit with code 1.
    Exit {
        /// Exit code.
        code: i32,
    },
    /// Hard-exit with code 0 (from the cancel/escape handler).
    Cancel {
        /// Exit code.
        code: i32,
    },
}

/// Build the option list (Accept first, Exit second).
pub fn build_options() -> Vec<SelectOption<DevChannelsValue>> {
    vec![
        SelectOption::new(
            "I am using this for local development",
            DevChannelsValue::Accept,
        ),
        SelectOption::new("Exit", DevChannelsValue::Exit),
    ]
}

/// Reducer.
pub fn handle_event(value: DevChannelsValue) -> DevChannelsAction {
    match value {
        DevChannelsValue::Accept => DevChannelsAction::Accept,
        DevChannelsValue::Exit => DevChannelsAction::Exit { code: 1 },
    }
}

/// Cancel handler.
pub fn handle_cancel() -> DevChannelsAction {
    DevChannelsAction::Cancel { code: 0 }
}

/// Format a single channel entry as the dialog footer expects.
pub fn format_entry(entry: &ChannelEntryInput) -> String {
    match entry {
        ChannelEntryInput::Plugin { name, marketplace } => format!("plugin:{name}@{marketplace}"),
        ChannelEntryInput::Server { name } => format!("server:{name}"),
    }
}

/// Format the joined "Channels: …" line (entries joined with `", "`).
pub fn format_channel_list(entries: &[ChannelEntryInput]) -> String {
    entries
        .iter()
        .map(format_entry)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(name: &str, market: &str) -> ChannelEntryInput {
        ChannelEntryInput::Plugin {
            name: name.into(),
            marketplace: market.into(),
        }
    }

    fn server(name: &str) -> ChannelEntryInput {
        ChannelEntryInput::Server { name: name.into() }
    }

    #[test]
    fn title_and_color() {
        assert_eq!(TITLE, "WARNING: Loading development channels");
        assert_eq!(DIALOG_COLOR, DialogColor::Error);
    }

    #[test]
    fn warning_paragraphs_pinned() {
        assert!(WARNING_PARAGRAPH_1.contains("--dangerously-load-development-channels"));
        assert!(WARNING_PARAGRAPH_2.contains("--channels"));
    }

    #[test]
    fn build_options_order_accept_first() {
        let opts = build_options();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].value, DevChannelsValue::Accept);
        assert_eq!(opts[1].value, DevChannelsValue::Exit);
    }

    #[test]
    fn handle_accept() {
        assert_eq!(
            handle_event(DevChannelsValue::Accept),
            DevChannelsAction::Accept
        );
    }

    #[test]
    fn handle_exit() {
        assert_eq!(
            handle_event(DevChannelsValue::Exit),
            DevChannelsAction::Exit { code: 1 }
        );
    }

    #[test]
    fn handle_cancel_zero_code() {
        assert_eq!(handle_cancel(), DevChannelsAction::Cancel { code: 0 });
    }

    #[test]
    fn format_entry_plugin() {
        assert_eq!(
            format_entry(&plugin("foo", "official")),
            "plugin:foo@official"
        );
    }

    #[test]
    fn format_entry_server() {
        assert_eq!(format_entry(&server("bar")), "server:bar");
    }

    #[test]
    fn format_channel_list_joins_with_comma_space() {
        let list = vec![plugin("foo", "official"), server("bar")];
        assert_eq!(
            format_channel_list(&list),
            "plugin:foo@official, server:bar"
        );
    }

    #[test]
    fn format_channel_list_empty() {
        assert_eq!(format_channel_list(&[]), "");
    }

    #[test]
    fn format_channel_list_single() {
        assert_eq!(format_channel_list(&[server("only")]), "server:only");
    }
}
