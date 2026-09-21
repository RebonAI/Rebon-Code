//! Progressive discovery: the skills a session did not have when it started.
//!
//! One subscriber on the kernel's `turn-hooks` seat. After every completed
//! tool round it reads the paths the round's file tools touched, activates the
//! conditional skills whose `paths:` patterns those match, and scans for
//! `.rebon/skills/` directories that came into view — registering what it
//! finds in the same [`SkillRegistry`](crate::SkillRegistry) the `Skill` tool
//! resolves against, so the listing names it on the next round.
//!
//! It ran inside the engine's turn loop as `core/progressive-skill-discovery`
//! at [`Order::NORMAL`], and it runs there still: same rung, same phase, and
//! the state it needs now arrives in the turn's extension bag rather than as
//! two fields on the event. What changed is that turning this plugin off takes
//! the subscriber off the seat, so discovery stops with the tool that would
//! have invoked what it found.

use std::sync::Arc;

use rebon_core::query::QueryEvent;
use rebon_core::turn_hook::{
    Order, ToolRoundHookEvent, TurnHook, TurnHookContext, TurnHookFuture, TurnHookSeat,
};
use rebon_kernel::{Context, KernelError};

use crate::loader::{extract_file_paths, SkillState};
use crate::skill::SkillContext;

/// Subscriber id. Keeps the `core/` prefix it had inside the engine so an
/// existing log line or ordering expectation still reads the same.
pub const PROGRESSIVE_SKILL_DISCOVERY_HOOK_ID: &str = "core/progressive-skill-discovery";

pub struct ProgressiveSkillDiscoveryHook;

impl TurnHook for ProgressiveSkillDiscoveryHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

    fn on_tool_round(&self, event: &ToolRoundHookEvent<'_>) -> Option<TurnHookFuture> {
        let skill = event.extension::<SkillContext>()?;
        let skill_state = skill.discovery.as_ref()?.clone();
        let skill_registry = skill.registry.as_ref()?.clone();
        let tool_uses: Vec<(&str, &serde_json::Value)> = event
            .message
            .tool_uses()
            .map(|tool_use| (tool_use.name.as_str(), &tool_use.input))
            .collect();
        let touched_paths = extract_file_paths(&tool_uses);
        if touched_paths.is_empty() {
            return None;
        }

        Some(Box::pin(async move {
            let _ =
                SkillState::on_files_touched(&skill_state, &touched_paths, &skill_registry).await;
            TurnHookContext::default()
        }))
    }
}

/// Put the subscriber on the seat for as long as this plugin is loaded.
pub fn subscribe(ctx: &Context, seat: &Arc<TurnHookSeat>) -> Result<(), KernelError> {
    seat.subscribe_scoped(
        ctx,
        PROGRESSIVE_SKILL_DISCOVERY_HOOK_ID,
        Order::NORMAL,
        Arc::new(ProgressiveSkillDiscoveryHook),
    )
}
