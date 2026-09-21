//! The durable-memory store and the `/memory` browser's pure logic.
//!
//! Four consumers reach into it — the prompt seat to render a prompt section,
//! the file tools to recognise an auto-memory path and format a notification,
//! the ACP server to list what a session loaded, the `agents` plugin to append
//! a sub-agent's memory — and each does so through a seat or a seam, so the
//! code has nothing left below it and belongs to the plugin that owns the
//! feature.
//!
//! Two halves, neither of which touches the outside world on its own:
//!
//! * **The store.** [`prompt`] renders the auto-`MEMORY.md` section and
//!   resolves what the entrypoints contain; [`save`], [`recall`], [`scan`]
//!   and [`surfacing`] are the read/write path behind `SaveMemory`;
//!   [`settings`] is the per-project `autoMemoryEnabled` switch;
//!   [`agent_memory`] is the per-agent variant; [`loaded_files`] is the list
//!   `/memory` and `/context` print; [`update_notification`] formats
//!   `Memory updated in … · /memory to edit`.
//! * **The browser.** [`option_list`], [`label`], [`description`],
//!   [`folder_options`], [`initial_path`], [`focus_state`],
//!   [`open_folder_value`], [`dream_status`], [`relative_path`], [`age`] and
//!   [`memory_fs`] are the plain-data transformations behind the `/memory`
//!   file picker. They produce strings, structs and reducer states; the
//!   rendering, the filesystem reads and the settings writes stay with their
//!   callers.
//!
//! Where the memory directories *are* is not here: that is
//! [`rebon_session::memory_paths`], because it is a layout fact keyed the
//! same way a session's transcript is, and the file tools have to recognise
//! those paths without depending on this feature.

#[cfg(test)]
pub(crate) mod test_env;

pub mod age;
pub mod agent_memory;
pub mod description;
pub mod dream_status;
pub mod focus_state;
pub mod folder_options;
pub mod initial_path;
pub mod label;
pub mod loaded_files;
pub mod memory_fs;
pub mod open_folder_value;
pub mod option_list;
pub mod prompt;
pub mod recall;
pub mod relative_path;
pub mod save;
pub mod scan;
pub mod settings;
pub mod surfacing;
pub mod update_notification;
