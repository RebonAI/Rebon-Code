//! What the task and team surfaces show, with nothing that paints it.
//!
//! The runtime next door ([`crate::runtime`]) says what a task *is* — its
//! snapshot, its lifecycle, its live event journal. This module says what
//! a person sees of it: the one-line label for a task row, the detail
//! panes for each of the six task kinds, the list and dialog reducers that
//! answer arrow keys, the pill row in the footer, the teams overlay.
//!
//! It carries no terminal type. A reducer takes a
//! [`rebon_dialog::model::DialogKey`] and returns an outcome; a projector
//! returns `String`s, `Vec`s and enums. Whichever surface owns the pixels —
//! the ratatui renderer, the desktop app — reads those and
//! paints. That is what lets the same projection answer for a terminal
//! overlay and a desktop panel without either one learning the other's
//! widget vocabulary.
//!
//! Two halves, by the surface they serve:
//!
//! * [`tasks`] — the background-task surfaces: `/tasks`, the Agent View
//!   rows, the per-kind detail panes, the footer pills.
//! * [`teams`] — the `/teams` overlay and the teammate-count footer line.
//!
//! The tools that create the tasks and the projections that display them
//! live on the same side of one plugin boundary. Turning this plugin off
//! now takes the surfaces off with the tools.

pub mod background_tasks_dialog;
pub mod commands;
pub mod panel_rows;
pub mod task_activity;
pub mod tasks;
pub mod tasks_view;
pub mod team_panes;
pub mod teams;
pub mod teams_dialog;
pub mod teams_view;
