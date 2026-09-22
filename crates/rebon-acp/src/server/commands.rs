use rebon_proto::types::SlashCommand;

/// Commands the ACP server implements.
///
/// Read from the shared catalog rather than listed here. This file used to
/// carry two lists of its own: this one, and a wider `default_slash_commands()`
/// that a terminal picker seeded from — a copy of a list that was duplicated
/// in other front-ends as well. The picker now builds from the catalog
/// directly, so that second list is gone.
///
/// The set is narrow on purpose: an editor is told what the server actually
/// intercepts, and advertising more produces a command that shows up in the
/// editor's picker and does nothing.
pub fn acp_advertised_slash_commands() -> Vec<SlashCommand> {
    rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Acp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eight below are the compiled-in table. `/memory` is advertised
    /// on this surface too, but it is the memory plugin's command and only
    /// exists once the kernel has booted, which this crate's tests never
    /// do; the booted list is pinned by the harness test
    /// `surface_bits_match_the_lists_they_replaced`.
    #[test]
    fn acp_advertised_commands_are_exactly_the_implemented_set() {
        let commands = acp_advertised_slash_commands();
        let mut names = commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>();
        // Order follows the catalog's menu order now, so compare the set.
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "codemode",
                "context",
                "cost",
                "doctor",
                "hooks",
                "mcp",
                "status",
                "ultrawork",
            ]
        );
        let ultrawork = commands
            .iter()
            .find(|command| command.name == "ultrawork")
            .expect("ultrawork is advertised");
        assert_eq!(ultrawork.aliases, ["ulw"]);
    }

    /// The catalog keeps the commands ACP does not advertise. Narrowing what
    /// goes over the wire must not narrow what a local session can run — the
    /// two used to be separate lists precisely so this could not happen, and
    /// now they are one list plus a filter.
    #[test]
    fn the_catalog_keeps_what_acp_does_not_advertise() {
        for required in ["help", "clear", "settings", "effort", "prune", "review"] {
            let spec = rebon_slash_commands::find(required)
                .unwrap_or_else(|| panic!("missing /{required}"));
            assert!(
                spec.available_on(rebon_slash_commands::Surface::Tui),
                "/{required} must still run locally"
            );
        }
    }
}
