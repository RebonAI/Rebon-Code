use rebon_permissions::PermissionMode;

use crate::tui::app::AppState;

pub const PROMPT_TIP_ROTATE_INTERVAL_MS: u64 = 12_000;

pub const UPDATE_PROMPT_TOP_HINT_ID: &str = "update";
pub const MCP_PROMPT_TOP_HINT_ID: &str = "mcp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTopHintTone {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTopHint {
    pub id: String,
    pub text: String,
    pub priority: i16,
    pub tone: PromptTopHintTone,
}

impl PromptTopHint {
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        priority: i16,
        tone: PromptTopHintTone,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            priority,
            tone,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTopHintInput {
    pub prompt_empty: bool,
    pub output_complete: bool,
    pub modal_open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptTopHintRegistry {
    hints: Vec<PromptTopHint>,
}

impl PromptTopHintRegistry {
    pub fn register(&mut self, hint: PromptTopHint) {
        self.remove(&hint.id);
        if !hint.text.trim().is_empty() {
            self.hints.push(hint);
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<PromptTopHint> {
        let index = self.hints.iter().position(|hint| hint.id == id)?;
        Some(self.hints.remove(index))
    }

    pub fn resolve(&self, input: PromptTopHintInput) -> Option<&PromptTopHint> {
        if !input.prompt_empty || !input.output_complete || input.modal_open {
            return None;
        }

        self.hints
            .iter()
            .max_by(|a, b| a.priority.cmp(&b.priority).then_with(|| b.id.cmp(&a.id)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTipCandidates {
    pub key: String,
    pub tips: Vec<String>,
}

pub fn candidates_for_app(app: &AppState) -> PromptTipCandidates {
    if app.slash_picker.is_some() {
        return PromptTipCandidates::new(
            "slash-picker",
            [
                "Use ↑/↓ to choose a command, Enter to insert it.",
                "Keep typing to filter slash commands.",
            ],
        );
    }
    if app.at_mention_picker.is_some() {
        return PromptTipCandidates::new(
            "at-mention-picker",
            [
                "Use @ to attach files or mention teammates.",
                "Keep typing after @ to narrow file matches.",
            ],
        );
    }
    if app.pending_permission_view.is_some() {
        return PromptTipCandidates::new(
            "permission-pending",
            [
                "Review the permission request before continuing.",
                "Use the permission choices to allow, deny, or remember a decision.",
            ],
        );
    }
    if app.permission_mode != PermissionMode::Default {
        return PromptTipCandidates::new(
            format!("permission-mode:{}", app.permission_mode.as_wire()),
            [
                "shift+tab to cycle default, plan, accept edits, and auto modes.",
                "Permission mode changes affect the next tool calls.",
            ],
        );
    }
    if app.is_loading {
        return PromptTipCandidates::new(
            "loading",
            [
                "You can keep typing while the agent works; Enter will queue it.",
                "Esc twice can open message selection while work is active.",
            ],
        );
    }
    if app.foregrounded_task_id.is_some() {
        return PromptTipCandidates::new(
            "foreground-agent",
            [
                "Messages go to the foregrounded agent until you return to main.",
                "Use the agent footer to switch back to the main session.",
            ],
        );
    }
    if app.coordinator_mode || !app.task_snapshots().is_empty() {
        return PromptTipCandidates::new(
            "coordinator-tasks",
            [
                "Use /tasks to inspect coordinator and background work.",
                "Coordinator agents can continue while you draft the next prompt.",
            ],
        );
    }
    if app.goal.as_ref().is_some_and(|goal| goal.is_active()) {
        return PromptTipCandidates::new(
            "active-goal",
            [
                "Use /goal to view or update the active goal.",
                "The active goal can continue across completed turns.",
            ],
        );
    }

    PromptTipCandidates::new(
        "idle-default",
        [
            "Type / for commands, @ for files, or ask a question.",
            "Paste code or logs directly; multi-line pastes are grouped.",
            "shift+tab cycles default, plan, accept edits, and auto modes.",
            "Press ← twice on an empty prompt to move this session into a worker that outlives the terminal.",
        ],
    )
}

impl PromptTipCandidates {
    fn new<const N: usize>(key: impl Into<String>, tips: [&str; N]) -> Self {
        Self {
            key: key.into(),
            tips: tips.into_iter().map(str::to_string).collect(),
        }
    }
}

pub fn rotate_tip(
    current_key: &mut String,
    current_index: &mut usize,
    next_rotate_at_ms: &mut u64,
    now_ms: u64,
    candidates: &PromptTipCandidates,
) -> Option<String> {
    let list_signature = candidates.tips.join("\u{1f}");
    let key = format!("{}\u{1e}{}", candidates.key, list_signature);
    if *current_key != key {
        *current_key = key;
        *current_index = 0;
        *next_rotate_at_ms = now_ms.saturating_add(PROMPT_TIP_ROTATE_INTERVAL_MS);
    } else if candidates.tips.len() > 1 && now_ms >= *next_rotate_at_ms {
        *current_index = (*current_index + 1) % candidates.tips.len();
        *next_rotate_at_ms = now_ms.saturating_add(PROMPT_TIP_ROTATE_INTERVAL_MS);
    } else if candidates.tips.len() <= 1 && now_ms >= *next_rotate_at_ms {
        *next_rotate_at_ms = now_ms.saturating_add(PROMPT_TIP_ROTATE_INTERVAL_MS);
    }

    candidates.tips.get(*current_index).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(key: &str, tips: &[&str]) -> PromptTipCandidates {
        PromptTipCandidates {
            key: key.into(),
            tips: tips.iter().map(|tip| tip.to_string()).collect(),
        }
    }

    #[test]
    fn rotation_is_stable_until_deadline() {
        let mut key = String::new();
        let mut index = 0;
        let mut deadline = 0;
        let tips = candidates("idle", &["one", "two"]);

        assert_eq!(
            rotate_tip(&mut key, &mut index, &mut deadline, 100, &tips),
            Some("one".into())
        );
        assert_eq!(
            rotate_tip(&mut key, &mut index, &mut deadline, 101, &tips),
            Some("one".into())
        );
        let rotate_at = deadline;
        assert_eq!(
            rotate_tip(&mut key, &mut index, &mut deadline, rotate_at, &tips),
            Some("two".into())
        );
    }

    #[test]
    fn context_change_resets_index_and_deadline() {
        let mut key = String::new();
        let mut index = 0;
        let mut deadline = 0;
        let first = candidates("idle", &["one", "two"]);
        let second = candidates("loading", &["loading tip"]);

        rotate_tip(&mut key, &mut index, &mut deadline, 0, &first);
        let first_rotate_at = deadline;
        rotate_tip(&mut key, &mut index, &mut deadline, first_rotate_at, &first);
        assert_eq!(index, 1);

        assert_eq!(
            rotate_tip(&mut key, &mut index, &mut deadline, 50, &second),
            Some("loading tip".into())
        );
        assert_eq!(index, 0);
        assert_eq!(deadline, 50 + PROMPT_TIP_ROTATE_INTERVAL_MS);
    }

    #[test]
    fn completed_goal_uses_idle_default_tips() {
        let mut app = AppState::default();
        let mut goal = crate::goal::GoalState::new_with_started_at("ship", 10);
        goal.mark_complete(Some("done".into()));
        app.goal = Some(goal);

        assert_eq!(candidates_for_app(&app).key, "idle-default");
    }

    #[test]
    fn single_candidate_does_not_advance_index() {
        let mut key = String::new();
        let mut index = 0;
        let mut deadline = 0;
        let tips = candidates("single", &["only"]);

        rotate_tip(&mut key, &mut index, &mut deadline, 0, &tips);
        let first_deadline = deadline;
        assert_eq!(
            rotate_tip(&mut key, &mut index, &mut deadline, first_deadline, &tips),
            Some("only".into())
        );
        assert_eq!(index, 0);
        assert!(deadline > first_deadline);
    }

    #[test]
    fn top_hint_registry_requires_empty_idle_prompt() {
        let mut registry = PromptTopHintRegistry::default();
        registry.register(PromptTopHint::new(
            "rate",
            "Rate this response",
            10,
            PromptTopHintTone::Info,
        ));

        assert!(registry
            .resolve(PromptTopHintInput {
                prompt_empty: true,
                output_complete: true,
                modal_open: false,
            })
            .is_some());
        assert!(registry
            .resolve(PromptTopHintInput {
                prompt_empty: false,
                output_complete: true,
                modal_open: false,
            })
            .is_none());
        assert!(registry
            .resolve(PromptTopHintInput {
                prompt_empty: true,
                output_complete: false,
                modal_open: false,
            })
            .is_none());
        assert!(registry
            .resolve(PromptTopHintInput {
                prompt_empty: true,
                output_complete: true,
                modal_open: true,
            })
            .is_none());
    }

    #[test]
    fn top_hint_registry_uses_priority_and_replaces_by_id() {
        let mut registry = PromptTopHintRegistry::default();
        registry.register(PromptTopHint::new(
            "rate",
            "Rate this response",
            10,
            PromptTopHintTone::Info,
        ));
        registry.register(PromptTopHint::new(
            UPDATE_PROMPT_TOP_HINT_ID,
            "Update available",
            100,
            PromptTopHintTone::Warning,
        ));
        registry.register(PromptTopHint::new(
            "rate",
            "Rate the last answer",
            120,
            PromptTopHintTone::Info,
        ));

        let hint = registry
            .resolve(PromptTopHintInput {
                prompt_empty: true,
                output_complete: true,
                modal_open: false,
            })
            .unwrap();
        assert_eq!(hint.id, "rate");
        assert_eq!(hint.text, "Rate the last answer");
    }
}
