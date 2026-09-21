//! Reading `/profile` off a composer line, and running what it says.

use std::path::Path;

use rebon_config::profile_store::{self, Profile};

use crate::apply::{apply_to_session, reset_tool_surface, save_current_session, ProfileSession};
use crate::field::set_profile_field;
use crate::render::{render_list, render_profile, usage_text};
use crate::{ProfileApplyOutcome, ProfileCommandResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileCommand {
    List,
    Show(String),
    /// `use <name>`; the flag is the user re-stating that a profile turning
    /// permission prompts off may do so.
    Use {
        name: String,
        allow_bypass: bool,
    },
    Save {
        name: String,
        description: Option<String>,
    },
    /// `set <name> <field> <value>` — write one field of one profile.
    Set {
        name: String,
        field: String,
        value: String,
    },
    Remove(String),
    /// Put the tool surface back to this session's default.
    Reset,
    Help,
}

pub fn parse_profile_args(text: &str) -> ProfileCommand {
    let rest = rebon_slash_commands::strip_command_prefix(text.trim_end(), "profile")
        .map(|rest| rest.trim_start_matches([' ', ':']).trim())
        .unwrap_or("");
    if rest.is_empty() {
        return ProfileCommand::List;
    }
    let mut parts = rest.split_whitespace();
    let verb = parts.next().unwrap_or_default();
    let rest_of_line = rest[verb.len()..].trim();
    match verb.to_lowercase().as_str() {
        "list" | "ls" => ProfileCommand::List,
        "show" | "cat" => ProfileCommand::Show(rest_of_line.to_string()),
        "remove" | "rm" | "delete" => ProfileCommand::Remove(rest_of_line.to_string()),
        "reset" | "clear" => ProfileCommand::Reset,
        "help" => ProfileCommand::Help,
        "save" => {
            let mut save_parts = rest_of_line.splitn(2, char::is_whitespace);
            let name = save_parts.next().unwrap_or_default().to_string();
            let description = save_parts
                .next()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            ProfileCommand::Save { name, description }
        }
        "set" | "edit" => {
            let (name, rest) = split_first_word(rest_of_line);
            let (field, value) = split_first_word(rest);
            ProfileCommand::Set {
                name: name.to_string(),
                field: field.to_string(),
                // The rest of the line, so a description keeps its spaces and
                // a tool list may be written with them.
                value: value.trim_end().to_string(),
            }
        }
        "use" | "switch" => parse_use_target(rest_of_line),
        // `/profile writing` is `/profile use writing`. The bare form is what
        // anyone reaches for, and reserving it for a verb nobody typed would
        // make the common case the long one.
        _ => parse_use_target(rest),
    }
}

/// Split off the first whitespace-delimited word, returning it and the
/// remainder with its leading whitespace already eaten.
fn split_first_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.find(char::is_whitespace) {
        Some(end) => (&text[..end], text[end..].trim_start()),
        None => (text, ""),
    }
}

fn parse_use_target(text: &str) -> ProfileCommand {
    let mut name = String::new();
    let mut allow_bypass = false;
    for token in text.split_whitespace() {
        if token.eq_ignore_ascii_case("--allow-bypass") {
            allow_bypass = true;
        } else if name.is_empty() {
            name = token.to_string();
        }
    }
    ProfileCommand::Use { name, allow_bypass }
}

/// Whether running this command would hand the session to another agent.
///
/// Asked before anything is applied, because a backend switch carries
/// restrictions a provider or permission-mode switch does not: it cannot
/// happen mid-prompt, and it cannot happen at all while the session is
/// attached somewhere else. `/backend` turns those cases away by name; a
/// profile reaches the same switch by a different road, and has to be turned
/// away for the same reasons rather than sliding past them.
pub fn switches_agent(config_dir: &Path, text: &str) -> bool {
    let ProfileCommand::Use { name, .. } = parse_profile_args(text) else {
        return false;
    };
    profile_store::load(config_dir, &name).is_ok_and(|profile| profile.agent.is_some())
}

/// Run one `/profile` line against a live session.
///
/// The runtime re-resolve is returned rather than performed —
/// see [`ProfileApplyOutcome::runtime_refresh`].
pub fn handle_profile_command(
    session: &mut dyn ProfileSession,
    config_dir: &Path,
    text: &str,
) -> ProfileApplyOutcome {
    let done = |result| ProfileApplyOutcome::nothing_applied(result);
    match parse_profile_args(text) {
        ProfileCommand::Help => done(ProfileCommandResult::ok(usage_text())),
        ProfileCommand::List => done(ProfileCommandResult::ok(render_list(config_dir))),
        ProfileCommand::Show(name) => done(match profile_store::load(config_dir, &name) {
            Ok(profile) => ProfileCommandResult::ok(render_profile(config_dir, &profile)),
            Err(err) => ProfileCommandResult::err(err.to_string()),
        }),
        ProfileCommand::Remove(name) if name.trim().is_empty() => {
            done(ProfileCommandResult::err("Usage: /profile remove <name>"))
        }
        ProfileCommand::Remove(name) => done(match profile_store::remove(config_dir, &name) {
            Ok(true) => ProfileCommandResult::ok(format!("Profile \"{name}\" removed.")),
            Ok(false) => ProfileCommandResult::err(format!("Profile \"{name}\" not found.")),
            Err(err) => ProfileCommandResult::err(err.to_string()),
        }),
        ProfileCommand::Save { name, description } => done(save_current_session(
            config_dir,
            session,
            &name,
            description,
        )),
        ProfileCommand::Set { name, field, value } => done(set_profile_field(
            config_dir, session, &name, &field, &value,
        )),
        ProfileCommand::Reset => done(reset_tool_surface(session)),
        ProfileCommand::Use { name, allow_bypass } => {
            if name.is_empty() {
                return done(ProfileCommandResult::err(usage_text()));
            }
            let profile: Profile = match profile_store::load(config_dir, &name) {
                Ok(profile) => profile,
                Err(err) => return done(ProfileCommandResult::err(err.to_string())),
            };
            apply_to_session(session, config_dir, &profile, allow_bypass)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_means_switch_to_it() {
        assert_eq!(
            parse_profile_args("/profile writing"),
            ProfileCommand::Use {
                name: "writing".into(),
                allow_bypass: false
            }
        );
        assert_eq!(
            parse_profile_args("/profile use writing"),
            ProfileCommand::Use {
                name: "writing".into(),
                allow_bypass: false
            }
        );
        assert_eq!(parse_profile_args("/profile"), ProfileCommand::List);
        assert_eq!(parse_profile_args("/profile list"), ProfileCommand::List);
    }

    #[test]
    fn the_bypass_flag_is_recognised_on_either_side_of_the_name() {
        let expected = ProfileCommand::Use {
            name: "risky".into(),
            allow_bypass: true,
        };
        assert_eq!(
            parse_profile_args("/profile use risky --allow-bypass"),
            expected
        );
        assert_eq!(
            parse_profile_args("/profile --allow-bypass risky"),
            expected
        );
        // A profile literally called `--allow-bypass` is not a thing; the flag
        // never becomes the name.
        assert_eq!(
            parse_profile_args("/profile use --allow-bypass"),
            ProfileCommand::Use {
                name: String::new(),
                allow_bypass: true
            }
        );
    }

    #[test]
    fn save_takes_the_rest_of_the_line_as_the_description() {
        assert_eq!(
            parse_profile_args("/profile save writing narrow tools, cheap model"),
            ProfileCommand::Save {
                name: "writing".into(),
                description: Some("narrow tools, cheap model".into())
            }
        );
        assert_eq!(
            parse_profile_args("/profile save writing"),
            ProfileCommand::Save {
                name: "writing".into(),
                description: None
            }
        );
    }

    #[test]
    fn set_takes_a_name_a_field_and_the_rest_of_the_line() {
        assert_eq!(
            parse_profile_args("/profile set writing model vendor-flash"),
            ProfileCommand::Set {
                name: "writing".into(),
                field: "model".into(),
                value: "vendor-flash".into()
            }
        );
        // The value keeps its spaces: a description is one of the fields.
        assert_eq!(
            parse_profile_args("/profile set writing description  narrow tools, cheap model  "),
            ProfileCommand::Set {
                name: "writing".into(),
                field: "description".into(),
                value: "narrow tools, cheap model".into()
            }
        );
        assert_eq!(
            parse_profile_args("/profile set writing agent -"),
            ProfileCommand::Set {
                name: "writing".into(),
                field: "agent".into(),
                value: "-".into()
            }
        );
        // A missing field is not a switch to a profile called "set".
        assert_eq!(
            parse_profile_args("/profile set writing"),
            ProfileCommand::Set {
                name: "writing".into(),
                field: String::new(),
                value: String::new()
            }
        );
    }

    #[test]
    fn show_and_remove_and_reset_parse() {
        assert_eq!(
            parse_profile_args("/profile show writing"),
            ProfileCommand::Show("writing".into())
        );
        assert_eq!(
            parse_profile_args("/profile rm writing"),
            ProfileCommand::Remove("writing".into())
        );
        assert_eq!(parse_profile_args("/profile reset"), ProfileCommand::Reset);
    }

    /// A leading space is how you talk *about* a command, and the catalog's
    /// prefix rule is the one place that decides it. Pinned here because
    /// `/profile` reaches it through a wrapper rather than directly.
    #[test]
    fn a_leading_space_is_prompt_text_and_case_never_decides() {
        assert_eq!(
            parse_profile_args(" /profile writing"),
            ProfileCommand::List
        );
        assert_eq!(
            parse_profile_args("/Profile show writing"),
            ProfileCommand::Show("writing".into())
        );
    }
}
