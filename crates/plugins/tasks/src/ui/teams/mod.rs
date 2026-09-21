//! State-machine models for the two team surfaces:
//!
//! * the teammate count + hint footer.
//! * the `/teams` overlay with teammate list/detail navigation and actions.
//!
//! ## What is in this module
//!
//! * [`team_status`] - summary projection covering teammate count, pluralization, and hint gating.
//! * [`teams_dialog`] - dialog state machine, teammate selection, mode cycling, and action command shapes.
//!
//! ## Outbound seam shapes
//!
//! Rendering and side-effect concerns are modeled as input structs or command enums so that the consumer drives overlays, keybinds, tmux calls, and `AppState`.

#![deny(missing_docs)]

pub mod team_status;
pub mod teams_dialog;

pub use team_status::{render_team_status, TeamStatusDisplay, TeamStatusTeammate};
pub use teams_dialog::{
    DialogLevel, PermissionMode, TeammateActivity, TeamsDialogAction, TeamsDialogCommand,
    TeamsDialogOutcome, TeamsDialogState, TeamsDialogTeammate,
};
