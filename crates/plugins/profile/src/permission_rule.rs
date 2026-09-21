//! What a profile proposal asks of the permission layer.
//!
//! One claim, made twice: `ProfileSwitch` and `ProfileSave` are tool calls
//! whose **entire output is a user decision**, so a mode that resolves the
//! prompt on the user's behalf does not save them a click — it deletes the
//! decision.
//!
//! The claim is [`DecisionScope::EvenUnderBypass`], which is stronger than
//! anything else the engine grants. Every other tool's contract under
//! `bypassPermissions` is "run what the model asked for"; these two have no
//! content of their own, and running one unprompted would hand the model the
//! thing the profile design rules out — changing the permission mode with
//! nobody consulted.
//!
//! `dontAsk` is the other side of that. It promises never to prompt, so it
//! fails closed and denies instead, and the reason it gives says the one thing
//! the model needs to hear: the user can still change profile themselves, and
//! resubmitting will be denied again.
//!
//! The rule reaches the engine through the `permission-rules` seat, so
//! nothing in the engine knows these two tools by name. Turning this plugin
//! off takes the two tools off the tool seat and the carve-out off with them,
//! rather than leaving a rule behind about calls that can no longer be
//! made.

use rebon_core::permission_seat::{DecisionScope, PermissionRule};
use rebon_tool::ToolContext;

use crate::{PROFILE_SAVE_TOOL_NAME, PROFILE_SWITCH_TOOL_NAME};

/// Whether `tool_name` is one of the two profile proposals.
fn is_profile_proposal(tool_name: &str) -> bool {
    tool_name == PROFILE_SWITCH_TOOL_NAME || tool_name == PROFILE_SAVE_TOOL_NAME
}

/// The profile feature's entry on the `permission-rules` seat.
pub struct ProfilePermissionRule;

impl PermissionRule for ProfilePermissionRule {
    fn requires_user_decision(
        &self,
        tool_name: &str,
        _context: &ToolContext,
    ) -> Option<DecisionScope> {
        is_profile_proposal(tool_name).then_some(DecisionScope::EvenUnderBypass)
    }

    fn deny_reason_when_never_prompting(&self, tool_name: &str) -> Option<String> {
        is_profile_proposal(tool_name).then(|| {
            format!(
                "`dontAsk` permission mode denied this {tool_name} call: it exists to put a decision in front of the user and this session never prompts. The user can change profile themselves with `/profile`. Resubmitting the same call will be denied again."
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim itself: both tools, at the strength that outranks
    /// `bypassPermissions`, and nothing said about any other call.
    #[test]
    fn both_proposals_must_reach_the_user_even_under_bypass() {
        let context = ToolContext::new();
        for tool_name in [PROFILE_SWITCH_TOOL_NAME, PROFILE_SAVE_TOOL_NAME] {
            assert_eq!(
                ProfilePermissionRule.requires_user_decision(tool_name, &context),
                Some(DecisionScope::EvenUnderBypass),
                "{tool_name}"
            );
        }
        assert_eq!(
            ProfilePermissionRule.requires_user_decision("Read", &context),
            None
        );
    }

    /// The `dontAsk` refusal points at the thing the user can still do, and
    /// says retrying is pointless — the model must not sit in a loop against
    /// a mode.
    #[test]
    fn the_never_prompting_refusal_names_the_command_the_user_still_has() {
        for tool_name in [PROFILE_SWITCH_TOOL_NAME, PROFILE_SAVE_TOOL_NAME] {
            let reason = ProfilePermissionRule
                .deny_reason_when_never_prompting(tool_name)
                .expect("a profile proposal has its own refusal");
            assert!(reason.contains("dontAsk"), "{reason}");
            assert!(reason.contains(tool_name), "{reason}");
            assert!(reason.contains("/profile"), "{reason}");
            assert!(reason.contains("denied again"), "{reason}");
        }
        assert_eq!(
            ProfilePermissionRule.deny_reason_when_never_prompting("Read"),
            None
        );
    }
}
