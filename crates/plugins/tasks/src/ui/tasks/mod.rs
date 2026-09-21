//! Background-task behaviour models: pure projections and reducers for
//! every background-task surface.
//!
//! * [`tool_activity`] — projection from a `ToolActivity` event to a display
//!   label.
//! * [`status_utils`] — pure helpers
//!   ([`status_utils::is_terminal_status`], [`status_utils::task_status_icon`],
//!   [`status_utils::task_status_color`], [`status_utils::describe_teammate_activity`],
//!   [`status_utils::should_hide_tasks_footer`]).
//! * [`shell_progress`] — pure projection from a shell task's status to a
//!   [`shell_progress::ShellProgressLine`] (label + semantic color). Includes
//!   [`shell_progress::task_status_text`].
//! * [`remote_progress`] — [`remote_progress::format_review_stage_counts`]
//!   (the canonical stage-counts string), [`remote_progress::SmoothCount`]
//!   (the +1/frame tween reducer used by the rainbow line), and the
//!   [`remote_progress::format_remote_session_progress`] projection.
//! * [`async_agent_detail`] — pure projector
//!   [`async_agent_detail::build_async_agent_detail`] + keyboard reducer in
//!   [`async_agent_detail::AsyncAgentDetailEvent`].
//! * [`dream_detail`] — pure projector [`dream_detail::build_dream_detail`]
//!   (subtitle, status pill, turns trimmed to `VISIBLE_TURNS = 6`).
//! * [`teammate_detail`] — pure projector
//!   [`teammate_detail::build_teammate_detail`].
//! * [`background_task`] — one single-line projection per task kind
//!   (`background_task::format_*_line`), all producing a
//!   [`background_task::BackgroundTaskLine`].
//! * [`task_list`] — list reducer ([`task_list::PrioritizedTasks`]) +
//!   recent-completion TTL window ([`task_list::CompletionTracker`]) +
//!   prioritization sort + per-item rendering helper
//!   ([`task_list::build_task_item`]).
//! * [`shell_detail`] — output tail line extraction
//!   ([`shell_detail::extract_tail_lines`]), subtitle/status row builders, key
//!   reducer.
//! * [`background_status`] — pill row reducer
//!   ([`background_status::build_pill_row`]) plus the per-task-kind
//!   dispatch.
//! * [`tasks_dialog`] — the layout builder
//!   ([`tasks_dialog::build_dialog_layout`]), the mode + selection reducer
//!   ([`tasks_dialog::handle_tasks_dialog_event`]) and the per-detail
//!   back/forward routing.
//! * [`remote_detail`] — review stage labels, plan/output projection, and the
//!   navigation reducer ([`remote_detail::RemoteDetailState`]).
//!
//! ## Test policy
//!
//! Every module pins behaviour with a comprehensive test
//! table covering: success path, ordering errors, missing/malformed
//! params, unknown variants, edge cases, and round-trip where relevant.
//!
//! ## Outbound seams (each modeled as a small trait or pre-built input)
//!
//! * **Tool lookup** — the [`tool_activity::ToolRegistry`] trait returns
//!   `Option<String>` already projected to the final user-facing label, so
//!   renderer primitives do not leak into the model.
//! * **The task runtime** — reading the task list and the
//!   cancel / foreground / enter-teammate-view mutations. The model
//!   surfaces *events* (e.g., [`tasks_dialog::TasksDialogEvent`]) and the
//!   consumer routes them to the runtime.
//! * **Settings** — no projector here reads settings; the consumer
//!   resolves any preference before calling in.
//! * **Agent colors** — the theme color for an agent in the task list /
//!   background status row is a value on [`task_list::TaskItemInput`].
//! * **Terminal size** — widths arrive as value parameters.
//! * **Key handling** — key events are driven by the caller; each reducer
//!   exposes an event enum.
//! * **Rendering** — NOT a trait. The model produces plain `String`s,
//!   `Vec<…>` shapes, and `enum` variants; the consumer renders them.
//!
//! ## What has been deliberately deferred
//!
//! * **Reading the shell output file.** Hidden behind the
//!   `extract_tail_lines` projection: the consumer reads the file and
//!   passes the bytes in, as a `(content, bytes_total)` pair that becomes
//!   rendered lines plus the "incomplete" flag.
//! * **The animation clock.** The rainbow line is modeled as a reducer
//!   that takes `(time_ms, target, snap)` so the test matrix can pin the
//!   +1/frame tween without an animation loop.
//! * **Elapsed time.** Modeled as a value parameter.
//! * **Renderer primitives such as layout boxes, text nodes, panes, tabs,
//!   selects, and dialogs.** The model produces plain `String`s and structs.
//! * **Which teammates are active** — the model accepts the active-teammate
//!   set as a pre-built input.

#![deny(missing_docs)]

pub mod async_agent_detail;
pub mod background_status;
pub mod background_task;
pub mod common;
pub mod dream_detail;
pub mod remote_detail;
pub mod remote_progress;
pub mod shell_detail;
pub mod shell_progress;
pub mod status_utils;
pub mod task_list;
pub mod tasks_dialog;
pub mod teammate_detail;
pub mod tool_activity;
