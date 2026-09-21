//! Top-level spinner branch selection for spinner rows.
//!
//! [`spinner_branch`] decides which branch to return given the
//! current inputs. The decision tree:
//!
//! 1. **Brief mode** — when brief mode is active and not viewing a
//! teammate transcript → the `Brief` branch (or the idle display
//! when idle).
//! 2. **Leader idle, teammates running, not viewing teammate** —
//! static dim "Idle" display + optional teammate tree.
//! 3. **Foregrounded teammate is idle** — static dim "Worked for X"
//! or "Idle" display + optional teammate tree.
//! 4. **Default** — the animated spinner row + optional
//! teammate tree / expanded todos / tip / budget text.

/// Inputs to [`spinner_branch`], one field per branch decision.
/// Every field is a plain `bool` the caller sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpinnerBranchInputs {
    /// True when brief mode is active and no teammate transcript is
    /// being viewed. The caller resolves this from the brief-mode state,
    /// its environment gate and its feature flag.
    pub brief_mode_active: bool,
    /// Whether the leader is idle.
    pub leader_is_idle: bool,
    /// Whether any teammate is running. The caller derives it from the
    /// task list filtered by in-process teammate and status `running`.
    pub has_running_teammates: bool,
    /// Whether the foregrounded teammate is idle; `false` when no
    /// foregrounded teammate exists or it isn't idle.
    pub foregrounded_teammate_is_idle: bool,
    /// Whether a foregrounded teammate is present.
    pub has_foregrounded_teammate: bool,
    /// True when the user has expanded the spinner-tree view
    /// (the expanded teammates view).
    pub show_spinner_tree: bool,
    /// True when the user has expanded the todos view
    /// (the expanded tasks view).
    pub show_expanded_todos: bool,
    /// Whether every running teammate is idle.
    pub all_idle: bool,
}

/// The chosen branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinnerBranch {
    /// The single-line brief variant.
    Brief,
    /// Render the "leader idle while teammates run" static display.
    /// The teammate tree is shown when `show_spinner_tree`.
    LeaderIdleWithTeammates {
        /// Whether the teammate tree should also be rendered below.
        show_tree: bool,
    },
    /// Render the "viewing idle teammate" static display.
    ForegroundedTeammateIdle {
        /// Whether the teammate tree should also be rendered below.
        show_tree: bool,
        /// Whether to use the "Worked for X" past-tense text instead
        /// of "Idle".
        use_past_tense: bool,
    },
    /// Render the standard animated spinner row.
    AnimatedRow {
        /// Whether the teammate tree should be rendered below.
        show_tree: bool,
        /// Whether the expanded todos panel should be rendered.
        show_todos: bool,
    },
}

/// Which [`SpinnerBranch`] the module-level decision tree selects for `input`.
pub fn spinner_branch(input: SpinnerBranchInputs) -> SpinnerBranch {
    if input.brief_mode_active {
        return SpinnerBranch::Brief;
    }

    // Branch 2: leader idle, teammates running, not viewing teammate.
    if input.leader_is_idle && input.has_running_teammates && !input.has_foregrounded_teammate {
        return SpinnerBranch::LeaderIdleWithTeammates {
            show_tree: input.show_spinner_tree,
        };
    }

    // Branch 3: viewing idle teammate.
    if input.foregrounded_teammate_is_idle {
        return SpinnerBranch::ForegroundedTeammateIdle {
            show_tree: input.show_spinner_tree && input.has_running_teammates,
            use_past_tense: input.all_idle,
        };
    }

    // Branch 4: default animated row.
    SpinnerBranch::AnimatedRow {
        show_tree: input.show_spinner_tree && input.has_running_teammates,
        show_todos: input.show_expanded_todos && !input.show_spinner_tree,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SpinnerBranchInputs {
        SpinnerBranchInputs {
            brief_mode_active: false,
            leader_is_idle: false,
            has_running_teammates: false,
            foregrounded_teammate_is_idle: false,
            has_foregrounded_teammate: false,
            show_spinner_tree: false,
            show_expanded_todos: false,
            all_idle: false,
        }
    }

    #[test]
    fn brief_mode_takes_priority() {
        let mut i = base();
        i.brief_mode_active = true;
        // Even with everything else set, brief wins.
        i.leader_is_idle = true;
        i.has_running_teammates = true;
        assert_eq!(spinner_branch(i), SpinnerBranch::Brief);
    }

    #[test]
    fn default_branch_is_animated_row() {
        let r = spinner_branch(base());
        assert_eq!(
            r,
            SpinnerBranch::AnimatedRow {
                show_tree: false,
                show_todos: false,
            }
        );
    }

    #[test]
    fn leader_idle_with_teammates_branch() {
        let mut i = base();
        i.leader_is_idle = true;
        i.has_running_teammates = true;
        // Not viewing a teammate.
        let r = spinner_branch(i);
        assert_eq!(
            r,
            SpinnerBranch::LeaderIdleWithTeammates { show_tree: false }
        );
    }

    #[test]
    fn leader_idle_branch_disables_when_viewing_teammate() {
        let mut i = base();
        i.leader_is_idle = true;
        i.has_running_teammates = true;
        i.has_foregrounded_teammate = true;
        // Falls through to animated row, since teammate is not idle.
        let r = spinner_branch(i);
        match r {
            SpinnerBranch::AnimatedRow { .. } => {}
            other => panic!("expected AnimatedRow, got {other:?}"),
        }
    }

    #[test]
    fn foregrounded_teammate_idle_branch() {
        let mut i = base();
        i.has_foregrounded_teammate = true;
        i.foregrounded_teammate_is_idle = true;
        let r = spinner_branch(i);
        match r {
            SpinnerBranch::ForegroundedTeammateIdle { .. } => {}
            other => panic!("expected ForegroundedTeammateIdle, got {other:?}"),
        }
    }

    #[test]
    fn foregrounded_teammate_idle_uses_past_tense_when_all_idle() {
        let mut i = base();
        i.has_foregrounded_teammate = true;
        i.foregrounded_teammate_is_idle = true;
        i.all_idle = true;
        let r = spinner_branch(i);
        assert_eq!(
            r,
            SpinnerBranch::ForegroundedTeammateIdle {
                show_tree: false,
                use_past_tense: true,
            }
        );
    }

    #[test]
    fn animated_row_shows_tree_only_when_teammates_running() {
        let mut i = base();
        i.show_spinner_tree = true;
        // No running teammates → tree hidden.
        let r = spinner_branch(i);
        assert_eq!(
            r,
            SpinnerBranch::AnimatedRow {
                show_tree: false,
                show_todos: false,
            }
        );
    }

    #[test]
    fn animated_row_shows_tree_when_teammates_running() {
        let mut i = base();
        i.show_spinner_tree = true;
        i.has_running_teammates = true;
        let r = spinner_branch(i);
        assert_eq!(
            r,
            SpinnerBranch::AnimatedRow {
                show_tree: true,
                show_todos: false,
            }
        );
    }

    #[test]
    fn animated_row_todos_only_when_no_tree() {
        let mut i = base();
        i.show_expanded_todos = true;
        let r = spinner_branch(i);
        assert_eq!(
            r,
            SpinnerBranch::AnimatedRow {
                show_tree: false,
                show_todos: true,
            }
        );
    }

    #[test]
    fn animated_row_tree_takes_precedence_over_todos() {
        let mut i = base();
        i.show_spinner_tree = true;
        i.has_running_teammates = true;
        i.show_expanded_todos = true;
        let r = spinner_branch(i);
        assert_eq!(
            r,
            SpinnerBranch::AnimatedRow {
                show_tree: true,
                show_todos: false,
            }
        );
    }

    #[test]
    fn leader_idle_with_no_teammates_falls_through_to_animated() {
        let mut i = base();
        i.leader_is_idle = true;
        i.has_running_teammates = false;
        let r = spinner_branch(i);
        match r {
            SpinnerBranch::AnimatedRow { .. } => {}
            other => panic!("expected AnimatedRow, got {other:?}"),
        }
    }
}
