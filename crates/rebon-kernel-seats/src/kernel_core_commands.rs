//! `core-commands`: the Core plugin that owns the built-in slash commands.
//!
//! The table that used to be `rebon_slash_commands::catalog::CATALOG` lives here,
//! and instead of being a static every front end reads, it is *registered*:
//! `apply` provides the `command-registry` seat on the kernel root and
//! registers every built-in on it, so a plugin's commands and rebon's own go
//! through one door and one duplicate check. `rebon_slash_commands::{all,
//! find}` read the seat through the [`CatalogSource`] installed here, which
//! is how the readers across TUI / ACP / serve / profiles kept their call
//! shape.
//!
//! Handlers are all [`CommandHandler::Native`] carrying the catalog name:
//! each front end still maps the id to its own function
//! (`submit.rs::native_dispatch` in the TUI). The four commands the
//! terminal has no implementation for are [`CommandHandler::Explain`], so
//! typing one there says so instead of reaching the model as prompt text.

use std::sync::Arc;

pub use rebon_command_seat::{
    CommandArgs, CommandHandler, CommandSeat, CommandSeatService, RegisteredCommand,
    SeatCatalogSource,
};
use rebon_kernel::{Context, Kernel, KernelError, Plugin, PluginMeta, Service};
use rebon_slash_commands::{CommandSpec, Surface};

/// The plugin id, which is also its config key and the name `/plugins` shows.
pub const PLUGIN_ID: &str = "core-commands";

pub use rebon_slash_commands::DESKTOP_ONLY_EXPLANATION;

pub struct CoreCommandsPlugin {
    kernel: Arc<Kernel>,
}

impl CoreCommandsPlugin {
    pub fn new(kernel: Arc<Kernel>) -> Self {
        Self { kernel }
    }
}

impl Plugin for CoreCommandsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).provides(&[<CommandSeatService as Service>::NAME])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = CommandSeat::new();
        ctx.provide::<CommandSeatService>(seat.clone())?;
        for spec in builtin_command_table() {
            let handler = builtin_handler(&spec);
            seat.register(ctx, spec, handler)?;
        }
        // The readers' door. First installation wins, and the source looks
        // the seat up on every call, so a reload of this plugin — which
        // replaces the seat — is seen by the next read without reinstalling.
        let kernel = self.kernel.clone();
        rebon_slash_commands::install_catalog_source(Arc::new(SeatCatalogSource(move || {
            kernel.context().get::<CommandSeatService>()
        })));
        Ok(())
    }
}

/// How a built-in runs: natively under its own name, unless the terminal has
/// nothing to run it with.
fn builtin_handler(spec: &CommandSpec) -> CommandHandler {
    if spec.available_on(Surface::Tui) {
        CommandHandler::Native(spec.name.clone())
    } else {
        CommandHandler::Explain(DESKTOP_ONLY_EXPLANATION.into())
    }
}

/// The live seat, once the process kernel has booted.
///
/// `None` while `core-commands` is being reloaded, and before the assembly
/// layer has booted a kernel at all. Both answers mean the same thing to a
/// reader — `rebon_slash_commands::{all, find}` fall back to the static
/// built-in table — which is why this reads the slot rather than being handed
/// a kernel: the callers are front ends asking "what is registered right now",
/// and they already treat "nothing yet" as an answer.
///
/// Reading no longer *boots* — booting on the UI thread was the thing to
/// avoid, and every production caller reaches this holding a live session,
/// so the boot has happened.
pub fn command_seat() -> Option<Arc<CommandSeat>> {
    rebon_command_seat::process_command_seat()
}

/// The registered command a typed token names, with its handler.
pub fn find_command(token: &str) -> Option<RegisteredCommand> {
    command_seat()?.find(token)
}

pub use rebon_slash_commands::builtin_command_table;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    /// Two commands answering to one token means whichever comes first wins,
    /// silently. The seat refuses it at registration; this checks the table
    /// itself so the failure names the row, not the boot.
    #[test]
    fn no_token_names_two_commands() {
        let mut seen: HashSet<String> = HashSet::new();
        for spec in builtin_command_table() {
            for token in spec.spellings() {
                assert!(
                    seen.insert(token.to_ascii_lowercase()),
                    "`/{token}` names more than one command"
                );
            }
        }
    }

    /// A command nobody can run is a typo, not an entry.
    #[test]
    fn every_command_works_somewhere() {
        for spec in builtin_command_table() {
            assert!(
                spec.available_on(Surface::Tui) || spec.available_on(Surface::Desktop),
                "/{} is available nowhere",
                spec.name
            );
        }
    }

    /// A description is one line of a picker. The three lists the catalog
    /// replaced had drifted to three different lengths for the same command,
    /// and the longest ran to 100 characters — which a picker truncates,
    /// usually right before the part that mattered.
    #[test]
    fn descriptions_are_one_line_present_and_fit() {
        for spec in builtin_command_table() {
            assert!(
                !spec.description.is_empty(),
                "/{} has no description",
                spec.name
            );
            assert!(
                !spec.description.contains('\n'),
                "/{} has a multi-line description",
                spec.name
            );
            assert!(
                spec.description.len() <= 75,
                "/{} has a {}-character description: {:?}",
                spec.name,
                spec.description.len(),
                spec.description
            );
        }
    }

    /// A description says what the command does, not what one front end does
    /// with it.
    ///
    /// The desktop app's catalog described several commands by the settings
    /// page they opened — "Open Settings > Connectors" — which is not a thing
    /// that exists in a terminal. Merging the two lists is what surfaced it,
    /// and this is what keeps it from coming back the next time someone adds a
    /// command while looking at one front end.
    #[test]
    fn no_description_describes_one_front_end() {
        for spec in builtin_command_table() {
            // Case-insensitively, and it is the `>` that gives the leak away:
            // "Open settings" is a fine description of `/settings` itself,
            // while "Settings > Connectors" names a page one front end has.
            let description = spec.description.to_ascii_lowercase();
            for leak in ["settings >", "sidebar", "the window"] {
                assert!(
                    !description.contains(leak),
                    "/{}'s description names a front end: {:?}",
                    spec.name,
                    spec.description
                );
            }
        }
    }

    /// Menu aliases have to be unique too. Two commands answering to the same
    /// alias is a picker where one of them cannot be reached by typing.
    #[test]
    fn no_menu_alias_names_two_commands() {
        let mut seen = HashSet::new();
        for spec in builtin_command_table() {
            for alias in &spec.zh_aliases {
                assert!(
                    seen.insert(alias.to_string()),
                    "「{alias}」 finds more than one command"
                );
            }
        }
    }
}
