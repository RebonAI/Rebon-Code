//! Canonical coarse-grained projection of permission modes for settings UIs.
//!
//! Runtime decisions still apply configured rules, path guards, hooks, and the
//! auto-mode classifier to a concrete tool invocation. This projection reports
//! the mode-level fallback for representative capability classes; `Ask` for
//! auto-mode network or sensitive work intentionally reflects the live safety
//! gate rather than promising unconditional execution.

use crate::types::{PermissionBehavior, PermissionMode};

/// Stable capability classes exposed by permission settings surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionCapability {
    ReadFiles,
    WriteFiles,
    RunCommands,
    Network,
    Dangerous,
}

/// Project the canonical mode policy onto a coarse settings capability.
///
/// This is the single mode/capability mapping. Consumers must not reproduce it
/// in their UI layer. Concrete runtime decisions may be stricter when a rule,
/// working-directory guard, hook, or classifier applies.
pub fn permission_mode_capability_behavior(
    mode: PermissionMode,
    capability: PermissionCapability,
) -> PermissionBehavior {
    use PermissionBehavior::{Allow, Ask, Deny};
    use PermissionCapability::{Dangerous, Network, ReadFiles, RunCommands, WriteFiles};

    if capability == ReadFiles {
        return Allow;
    }
    if mode == PermissionMode::BypassPermissions {
        return Allow;
    }
    if matches!(mode, PermissionMode::DontAsk | PermissionMode::Plan) {
        return Deny;
    }
    // `AcceptEdits` allows this row through the engine's closed set of
    // file-edit tools (`Edit`, `Write`); `Auto` gets here via its own gate.
    // A row is coarser than the runtime: `WriteFiles` covers writing a file,
    // not every tool that could produce one.
    if capability == WriteFiles
        && matches!(mode, PermissionMode::AcceptEdits | PermissionMode::Auto)
    {
        return Allow;
    }
    if mode == PermissionMode::Auto && matches!(capability, RunCommands | Network | Dangerous) {
        // Auto mode evaluates these concrete invocations through safety rules
        // and/or its classifier. A coarse row cannot claim unconditional allow:
        // the auto-mode gate still stops a Bash call it classifies as sensitive.
        return Ask;
    }

    Ask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_projection_pins_runtime_policy_fallbacks() {
        assert_eq!(
            permission_mode_capability_behavior(
                PermissionMode::AcceptEdits,
                PermissionCapability::WriteFiles,
            ),
            PermissionBehavior::Allow
        );
        assert_eq!(
            permission_mode_capability_behavior(
                PermissionMode::AcceptEdits,
                PermissionCapability::RunCommands,
            ),
            PermissionBehavior::Ask
        );
        // Auto mode's Bash gate is three-state — a command the classifier reads
        // as sensitive is still stopped, so the row cannot promise "automatic".
        for capability in [
            PermissionCapability::RunCommands,
            PermissionCapability::Network,
            PermissionCapability::Dangerous,
        ] {
            assert_eq!(
                permission_mode_capability_behavior(PermissionMode::Auto, capability),
                PermissionBehavior::Ask
            );
        }
    }

    #[test]
    fn deny_and_bypass_modes_are_projected_consistently() {
        let capabilities = [
            PermissionCapability::ReadFiles,
            PermissionCapability::WriteFiles,
            PermissionCapability::RunCommands,
            PermissionCapability::Network,
            PermissionCapability::Dangerous,
        ];
        assert_eq!(
            capabilities.map(|capability| permission_mode_capability_behavior(
                PermissionMode::DontAsk,
                capability,
            )),
            [
                PermissionBehavior::Allow,
                PermissionBehavior::Deny,
                PermissionBehavior::Deny,
                PermissionBehavior::Deny,
                PermissionBehavior::Deny,
            ]
        );
        assert!(capabilities
            .into_iter()
            .all(|capability| permission_mode_capability_behavior(
                PermissionMode::BypassPermissions,
                capability,
            ) == PermissionBehavior::Allow));
    }
}
