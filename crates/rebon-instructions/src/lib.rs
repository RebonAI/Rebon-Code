//! # rebon-instructions — the project instruction documents
//!
//! The files a user writes to tell rebon how to work in a project, and
//! rebon reads back into every system prompt: the global `REBON.md` beside
//! the config home, every ancestor `REBON.md` / `.rebon/REBON.md` from the
//! filesystem root down to the cwd, `.rebon/rules/**/*.md`, the private
//! `REBON.local.md`, and the markdown `@` includes any of them pull in.
//!
//! **These are not memory.** Auto-memory is a `MEMORY.md` store the model
//! writes and a per-project switch decides whether to load; it lives in the
//! `memory` feature plugin and disappears with it. Instruction files are
//! what the *user* wrote, they are loaded whatever plugins are on, and they
//! are rendered by the engine's own prompt table — never by the feature
//! that happens to also read markdown out of a project.
//!
//! ## What is in this crate
//!
//! * [`instruction_files`] — the discovery walk itself, ordered exactly as
//!   the prompt renders it, with `@`-include expansion bounded by depth.
//! * [`memory_type`] — the `MemoryType` discriminant a discovered file
//!   carries (user / project / local / …), which is what decides the
//!   `Instruction text from <path> (<kind>)` label in the prompt.
//! * [`memory_file`] — `MemoryFileInfo`, one discovered document, plus the
//!   `ExtendedMemoryFileInfo` overlay the `/memory` browser lays on top.
//! * [`loaded_documents`] — [`loaded_documents::LoadedMemoryFile`] and the
//!   `loaded-documents` kernel seam a surface asks through for the full
//!   list a session loaded, instruction files *and* auto-memory.
//! * [`agent_documents`] — the `agent-memory-prompt` seam, for a sub-agent
//!   whose definition declares a memory scope.
//! * [`frontmatter`] — YAML frontmatter parsing shared by discovery (which
//!   normalizes `@` paths declared in it) and by the memory store.
//!
//! [`loaded_documents`] and [`agent_documents`] are seams rather than
//! implementations, and they are here for the same reason: each has a
//! consumer and a provider on opposite sides of the plugin boundary (the ACP
//! server and the `agents` plugin ask; the `memory` plugin answers), and
//! neither side may depend on the other. The shape of the answer has to sit
//! below both, and this is the crate that already owns "documents rebon puts
//! in front of the model".
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod agent_documents;
pub mod frontmatter;
pub mod instruction_files;
pub mod loaded_documents;
pub mod memory_file;
pub mod memory_type;

#[cfg(test)]
pub(crate) mod test_env;
