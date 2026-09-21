//! The `core-commands` seat, booted on the real plugin table.
//!
//! These moved out of `rebon_kernel_seats::kernel_core_commands`: the
//! seat is a plugin, and every question worth asking about it — what the table
//! registered, which surfaces a command is offered on, whether a command that
//! moved to its own plugin still reaches `/help` — is a question about the
//! *whole* built-in list. That list is `builtin_plugin_defs()`, which lives
//! here and names every plugin crate, so this is the only place the boot can
//! happen. The seat crate keeps the checks that read nothing but the static
//! table.
//!
//! One binary, one process kernel, and the tests share it on purpose: two of
//! them read what a sibling registered on the same seat and say so.

#[cfg(test)]
mod tests {
    use rebon_kernel_seats::kernel_core_commands::{
        builtin_command_table, command_seat, find_command, CommandArgs, CommandHandler,
        CommandSeat, DESKTOP_ONLY_EXPLANATION, PLUGIN_ID,
    };
    use rebon_slash_commands::{CommandSpec, Surface};
    use std::collections::HashSet;
    use std::sync::Arc;
    /// How many commands the built-in table holds.
    ///
    /// The static catalog had 54 the day it was deleted. Twelve have since
    /// moved to the plugin whose code actually runs them, and register on
    /// this same seat from there: `/profile` to `plugins/profile`, `/update`
    /// to `plugins/updater`, `/agents` to `plugins/agents`, `/tasks`,
    /// `/workflows` and `/teams` to `plugins/tasks`, `/onboarding`,
    /// `/migrate`, `/login` and `/logout` to `plugins/onboarding`, `/skills`
    /// to `plugins/skill`, and `/memory` to `plugins/memory`.
    ///
    /// What is left is the kernel's own: the session, the model, the
    /// permission layer, the plugin platform itself, and the terminal's
    /// local controls. A command that leaves this table is one whose
    /// implementation left `rebon-cli` / `rebon-harness` / `rebon-core`
    /// first.
    const BUILTIN_COMMAND_COUNT: usize = 42;

    /// The seat, on this binary's one process kernel.
    ///
    /// Booting is the harness's job and reading the seat is not, so the two
    /// are two calls now: `process_plugin_registry` builds the table this
    /// whole file is about, and `command_seat` reads what it registered.
    fn booted() -> Arc<CommandSeat> {
        let _ = rebon_harness::kernel_bootstrap::process_plugin_registry();
        command_seat().expect("core-commands is a Core plugin and always loads")
    }

    /// The seat holds exactly the old table, and the readers see it.
    #[test]
    fn core_commands_registers_the_whole_old_catalog() {
        let seat = booted();
        assert_eq!(builtin_command_table().len(), BUILTIN_COMMAND_COUNT);
        let registered: HashSet<String> = seat
            .all()
            .iter()
            .filter(|command| command.owner == PLUGIN_ID)
            .map(|command| command.spec.name.to_string())
            .collect();
        for spec in builtin_command_table() {
            assert!(
                registered.contains(spec.name.as_ref()),
                "/{} missing",
                spec.name
            );
        }
        assert!(rebon_slash_commands::catalog_source_installed());
        let through_readers: Vec<String> = rebon_slash_commands::all()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .collect();
        for spec in builtin_command_table() {
            assert!(through_readers.contains(&spec.name.to_string()));
        }
        assert_eq!(
            rebon_slash_commands::find("Quit").map(|spec| spec.name.to_string()),
            Some("exit".to_string())
        );
    }

    /// Every built-in the terminal can run is `Native(name)`; the rest
    /// explain themselves.
    #[test]
    fn built_in_handlers_are_native_under_their_own_name() {
        let seat = booted();
        for command in seat.all().iter().filter(|c| c.owner == PLUGIN_ID) {
            match &command.handler {
                CommandHandler::Native(id) => {
                    assert_eq!(id, &command.spec.name);
                    assert!(command.spec.available_on(Surface::Tui));
                }
                CommandHandler::Explain(text) => {
                    assert!(!command.spec.available_on(Surface::Tui));
                    assert_eq!(text, DESKTOP_ONLY_EXPLANATION);
                }
                other => panic!("/{} has handler {other:?}", command.spec.name),
            }
        }
        let explained: Vec<String> = seat
            .all()
            .into_iter()
            .filter(|c| matches!(c.handler, CommandHandler::Explain(_)))
            .map(|c| c.spec.name.to_string())
            .collect();
        assert_eq!(explained, ["shortcuts", "runtime", "automation", "devices"]);
    }

    /// The Tui set the static catalog offered, pinned so registration cannot
    /// quietly drop or add one.
    #[test]
    fn the_tui_surface_set_is_unchanged() {
        // The catalog reads the seat the booted kernel installed; tests run
        // in parallel, so each reader boots for itself.
        let seat = booted();
        // Only what core-commands registered: a sibling test registers its
        // own command on the same process seat, and must not count here.
        let mut offered: Vec<String> = seat
            .for_surface(Surface::Tui)
            .into_iter()
            .filter(|command| command.owner == PLUGIN_ID)
            .map(|command| command.spec.name.to_string())
            .collect();
        offered.sort_unstable();
        let mut expected = vec![
            "agent",
            "backend",
            "background",
            "background-agents",
            "ceo",
            "clear",
            "compact",
            "context",
            "cost",
            "doctor",
            "effort",
            "exit",
            "fast",
            "goal",
            "grill",
            "help",
            "hooks",
            "hosted",
            "kernel",
            "mcp",
            "model",
            "new",
            "permissions",
            "plugin",
            "provider",
            "prune",
            "resume",
            "review",
            "rewind",
            "run",
            "settings",
            "status",
            "statusline",
            "stop",
            "theme",
            "ultraplan",
            "ultrawork",
            "vim",
        ];
        expected.sort_unstable();
        assert_eq!(offered, expected);
    }

    /// The four hand-written lists the surface bits replaced.
    ///
    /// Read off the **seat**, not the built-in table, because that is what
    /// those lists were: everything a given surface offers. Twelve commands
    /// have since moved to the plugins that run them, and reading the table
    /// alone would have quietly shortened all four lists — `/memory` is on
    /// the mobile app and is session-control-forwarded, and losing either bit
    /// in the move is exactly the regression this test exists to catch.
    ///
    /// A sibling test registers `hello-from-test` on this same process seat
    /// and tests run in parallel, so it can be present here. It takes the
    /// `Surfaces::LOCAL` default and therefore carries none of the four bits
    /// below, which is why no owner filter is needed.
    #[test]
    fn surface_bits_match_the_lists_they_replaced() {
        let seat = booted();
        let names = |surface: Surface| -> Vec<String> {
            let mut names: Vec<String> = seat
                .for_surface(surface)
                .into_iter()
                .map(|command| command.spec.name.to_string())
                .collect();
            names.sort_unstable();
            names
        };
        let mut session_control = vec![
            "context",
            "memory",
            "doctor",
            "status",
            "cost",
            "mcp",
            "hooks",
            "compact",
            "prune",
            "permissions",
            "kernel",
            "backend",
        ];
        session_control.sort_unstable();
        assert_eq!(names(Surface::SessionControl), session_control);
        let mut mobile = vec![
            "status", "cost", "context", "memory", "mcp", "hooks", "doctor",
        ];
        mobile.sort_unstable();
        assert_eq!(names(Surface::Mobile), mobile);
        let mut web = vec![
            "help",
            "new",
            "clear",
            "status",
            "cost",
            "stop",
            "effort",
            "rewind",
            "settings",
            "model",
            "theme",
            "skills",
            "mcp",
            "plugin",
            "permissions",
            "shortcuts",
            "ultrawork",
            "agent",
            "backend",
            "compact",
            "context",
            "memory",
            "doctor",
            "hooks",
            // Registered by `plugins/agents` and `plugins/tasks`; their `WEB`
            // bit comes from the seat, not from the built-in table.
            "agents",
            "tasks",
            "workflows",
        ];
        web.sort_unstable();
        assert_eq!(names(Surface::Web), web);
        let mut acp = vec![
            "context",
            "cost",
            "doctor",
            "hooks",
            "mcp",
            "memory",
            "status",
            "ultrawork",
        ];
        acp.sort_unstable();
        assert_eq!(names(Surface::Acp), acp);
    }

    /// `/help` is a view of the seat, so a command that left the built-in
    /// table for the plugin that runs it has to still be on the help screen —
    /// with the metadata that screen renders.
    ///
    /// The rows are built by `rebon_slash_commands::help`, which reads
    /// [`rebon_slash_commands::for_surface`] when the screen opens rather than
    /// a list a front end sampled at start-up. This is the check that the
    /// reading reaches plugin rows: the label comes from the name and the
    /// hint, and the description picks up the aliases.
    #[test]
    fn help_still_lists_the_commands_that_moved_to_plugins() {
        let _seat = booted();
        let rows = rebon_slash_commands::help::catalog_command_rows(Surface::Tui);
        let described = |label: &str| -> String {
            rows.iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("{label} is missing from the help screen"))
                .description
                .clone()
        };

        // The three plugin commands `/help` has to keep listing, with the
        // descriptions word for word.
        assert_eq!(described("/skills"), "Manage available skills");
        assert_eq!(
            described("/memory"),
            "List loaded memory and instruction files"
        );
        assert_eq!(
            described("/migrate"),
            "Import Claude Code / Codex skills, agents, and commands"
        );

        // None of those three carries a hint or an alias, so two commands
        // moved in earlier batches stand in for the rest of the row shape:
        // `/tasks` names its alias, and `/update` puts its grammar in the
        // label. A move that dropped either field would show up here.
        assert_eq!(
            described("/tasks"),
            "Show background jobs and sub-agents (alias: /bg)"
        );
        assert_eq!(
            described("/update status|check|skip|channel <latest|stable>|auto <on|off|status>"),
            "Manage local Rebon update checks"
        );
    }

    /// The first entries are what an empty `/` shows, so they are the ones
    /// worth pinning: reordering them is a product decision, and this test is
    /// where someone making it has to say so.
    #[test]
    fn the_most_reached_for_commands_lead_the_menu() {
        // The catalog reads the seat the booted kernel installed; tests run
        // in parallel, so each reader boots for itself.
        let _seat = booted();
        let lead: Vec<String> = rebon_slash_commands::all()
            .iter()
            .take(4)
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(lead, ["help", "new", "clear", "status"]);
    }

    /// A Chinese alias that did parse would become a built-in and shadow any
    /// user skill carrying that name.
    #[test]
    fn no_chinese_alias_anywhere_parses_as_a_command() {
        let _ = booted();
        assert!(
            builtin_command_table()
                .iter()
                .any(|spec| !spec.zh_aliases.is_empty()),
            "nothing to check — did the aliases get dropped?"
        );
        for spec in builtin_command_table() {
            for alias in &spec.zh_aliases {
                assert!(
                    !spec.matches(alias),
                    "/{alias} must not name /{}",
                    spec.name
                );
                assert!(
                    matches!(
                        rebon_slash_commands::parse(&format!("/{alias}")),
                        rebon_slash_commands::Parsed::Unknown { .. }
                    ),
                    "/{alias} must stay available to a user skill"
                );
            }
        }
    }

    /// A plugin's command goes through the same door and leaves with its
    /// context; the readers see both directions.
    #[test]
    fn a_plugin_command_registers_and_unregisters_through_the_readers() {
        let seat = booted();
        let kernel = rebon_harness::kernel_bootstrap::process_kernel();
        let plugin = kernel.context().fork("test-plugin-hello");
        seat.register(
            &plugin,
            CommandSpec::new("hello-from-test", "Say hello").hint("<name>"),
            CommandHandler::Prompt(Arc::new(|args: &CommandArgs| {
                Ok(format!("Say hello to {}", args.rest))
            })),
        )
        .unwrap();
        assert!(rebon_slash_commands::find("hello-from-test").is_some());
        assert!(find_command("HELLO-FROM-TEST").is_some());
        plugin.dispose();
        assert!(rebon_slash_commands::find("hello-from-test").is_none());
        assert!(find_command("hello-from-test").is_none());
    }
}
