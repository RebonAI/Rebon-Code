//! # `surface` — pure logic for the agents-management surface
//!
//! Was the `rebon-agents` crate; it moved here whole when the agents
//! feature became a plugin, because everything it describes — where an
//! agent file lives, what a valid agent is, how the create wizard
//! steps — is this plugin's, and switching the plugin off should take
//! all of it with the `Agent` tool.
//!
//! Still pure logic, and still free of IO and of any terminal type: the
//! filesystem and the LLM call are injected seams, and the painting is
//! the surface's. What it may now name is the rest of the plugin, which
//! is what lets [`super::dialog`] hold the `/agents` panel here rather
//! than in the terminal.
//!
//! This module implements the agents surface as pure logic:
//!
//! * [`mode_state`] — the enum for the top-level agents-menu mode,
//!   plus [`types::AgentValidationResult`].
//! * [`utils::agent_source_display_name`] — pure display name for the
//!   source picker.
//! * [`validate::validate_agent_type`] +
//!   [`validate::validate_agent`] — pure validators with deterministic
//!   error/warning lists.
//! * [`agent_file`] — pure
//!   [`agent_file::format_agent_as_markdown`],
//!   [`agent_file::new_agent_file_path`],
//!   [`agent_file::actual_agent_file_path`],
//!   [`agent_file::new_relative_agent_file_path`],
//!   [`agent_file::actual_relative_agent_file_path`],
//!   plus the [`agent_file::AgentFs`] write/delete seam.
//! * [`generate`] — pure
//!   [`generate::build_generation_request`] (the prompt + system-prompt
//!   builder), [`generate::parse_generated_agent`] (the JSON-extracting
//!   parser), and the [`generate::AgentGenerator`] LLM seam. Also pins
//!   [`generate::AGENT_CREATION_SYSTEM_PROMPT`] +
//!   [`generate::AGENT_MEMORY_INSTRUCTIONS`].
//! * [`snapshot_dialog`] — pinned default-action constant.
//! * [`navigation_footer`] — pinned footer text.
//! * [`color_picker`] — selector reducer. Pure key-driven state
//!   machine.
//! * [`model_selector`] — selector reducer.
//! * [`wizard`] — the create-agent state machine: each step has its
//!   own per-step state + validator and the parent reducer threads
//!   them together.
//! * [`agent_editor`] — editor state (which field is being edited,
//!   dirty tracking).
//! * [`agent_detail::build_agent_detail`] — pure projector.
//! * [`agents_list`] — list-view filter + sort + selection reducer.
//! * [`tool_selector`] — tool filter + group + selection reducer.
//! * [`agents_menu`] — top-level menu state machine (the pure
//!   state-machine pieces only — see "Out of scope" below).
//! * [`progress_line::format_agent_progress_line`] — pure formatter.
//! * [`coordinator_status::build_coordinator_row`] — pure projector.
//!
//! ## Test policy
//!
//! Every module pins behaviour with a comprehensive test
//! table covering: success path, ordering errors, missing/malformed
//! params, unknown resources, edge cases, and round-trip where
//! relevant.
//!
//! ## Outbound seams (each modeled as a small trait or pre-built input)
//!
//! * **`AgentFs`** — abstracts the mkdir/write/delete IO for agent
//!   files. The crate never touches the filesystem directly; the
//!   consumer owns IO.
//! * **`AgentGenerator`** — abstracts the LLM call for agent
//!   generation. The crate builds the prompt, hands it to a generator,
//!   and parses the response. The actual API call is the consumer's
//!   problem.
//! * **Settings paths** — cwd, config home, managed-settings prefix,
//!   and project config dir name are provided to
//!   [`agent_file::AgentPathContext`] as value parameters — the crate
//!   does not read process state.
//! * **Tool resolution** — the crate exposes
//!   [`validate::ResolvedTools`] as a value parameter that the consumer
//!   computes upstream.
//! * **Agent registry** — the crate operates on a `&[AgentSummary]`
//!   slice the consumer provides.
//! * **Key handling** — the crate never dispatches key events
//!   directly; each reducer exposes an event enum the consumer drives.
//! * **Rendering** — NOT a trait. This module produces plain `String`s
//!   and `Vec<…>` shapes; [`super::dialog`] turns them into styled
//!   lines and the terminal paints those.
//!
//! ## Out of scope
//!
//! * **The actual filesystem write of agent .md files.** Hidden behind
//!   [`agent_file::AgentFs`].
//! * **The LLM call for agent generation.** Hidden behind
//!   [`generate::AgentGenerator`].
//! * **Keybinding / effect / state-cell lifecycles.** Each reducer
//!   exposes an event enum.
//! * **UI widget primitives** (boxes, text, panes, tabs, selects,
//!   dialogs). The crate produces plain `String`s and structs.
//! * **Agents-menu sub-flows that are pure UI plumbing** (e.g., view
//!   state that mirrors state already managed by the reducer). The
//!   state-machine transitions ARE implemented.
//! * **The full confirm-step dialog rendering** — only its validation
//!   and confirmation reducer are implemented. The wizard surfaces a
//!   pre-built confirmation summary.

#![deny(missing_docs)]

pub mod agent_detail;
pub mod agent_editor;
pub mod agent_file;
pub mod agents_list;
pub mod agents_menu;
pub mod color_picker;
pub mod coordinator_status;
pub mod generate;
pub mod mode_state;
pub mod model_selector;
pub mod navigation_footer;
pub mod progress_line;
pub mod snapshot_dialog;
pub mod tool_selector;
pub mod types;
pub mod utils;
pub mod validate;
pub mod wizard;
