//! The auto-memory system-prompt section, on the kernel's `prompt-sections`
//! seat.
//!
//! Until it moved here, this was rank 40 of the engine's own stable table, built
//! from a `DynamicPromptContext.memory_prompt` the engine resolved itself. It
//! is now a section this plugin registers at
//! [`Rung::Memory`], which is the same rank on the same plane, so the bytes a
//! model receives are unchanged while the plugin is on and the section simply
//! is not there while it is off.
//!
//! Three inputs decide the text, and all three arrive on the
//! [`PromptSubject`]:
//!
//! * the **cwd**, which resolves the project's memory directories and the
//!   `autoMemoryEnabled` setting that gates the whole section;
//! * whether the turn runs the **coordinator** contract, which selects
//!   coordinator-flavoured guidance;
//! * the **tools this turn offers**, because the guidance tells the model how
//!   to write a memory and there are three answers: `SaveMemory` if it is
//!   available, hand-editing if only `Write`/`Edit` are, and recall-only if
//!   neither is. A coordinator never hand-edits, so `can_write_memory` stays
//!   false there regardless.
//!
//! The provider also answers [`PromptSectionProvider::injected_files`]: the
//! `MEMORY.md` entrypoints this section quotes are seeded into the turn's
//! read-state cache, so `Edit` and `Write` do not refuse a file the prompt
//! just showed the model. The engine does the same for the project
//! instruction files it renders itself.

use rebon_core::prompt_seat::{
    InjectedPromptFile, PluginPromptSection, PromptSectionProvider, PromptSubject, Rung,
};

/// Detail name in the `/context` cost report, and the name the section sorts
/// by inside its rung. Unchanged from the engine's own report line.
pub const SECTION_NAME: &str = "memory prompt";

/// The `SaveMemory` tool's name, as the guidance branch asks about it.
const SAVE_MEMORY_TOOL: &str = crate::save_memory::SAVE_MEMORY_TOOL_NAME;

/// The auto-memory section provider.
pub struct MemoryPromptSections;

impl MemoryPromptSections {
    /// The options the section is built with for `subject`, or `None` when
    /// auto-memory is switched off for that project.
    ///
    /// Split out from [`PromptSectionProvider::sections_for`] so the gate and
    /// the three guidance branches can be asserted without assembling a
    /// prompt.
    pub fn options_for(
        subject: &PromptSubject,
    ) -> Option<crate::memory::prompt::MemoryPromptOptions> {
        if !crate::memory::settings::is_auto_memory_enabled(std::path::Path::new(&subject.cwd)) {
            return None;
        }
        Some(crate::memory::prompt::MemoryPromptOptions {
            is_coordinator: subject.coordinator,
            can_write_memory: !subject.coordinator
                && (subject.has_tool("Write") || subject.has_tool("Edit")),
            can_save_memory: subject.has_tool(SAVE_MEMORY_TOOL),
            budget: Some(crate::memory::prompt::MemoryPromptBudget::default_runtime()),
        })
    }
}

impl PromptSectionProvider for MemoryPromptSections {
    fn sections_for(&self, subject: &PromptSubject) -> Vec<PluginPromptSection> {
        let Some(options) = Self::options_for(subject) else {
            return Vec::new();
        };
        crate::memory::prompt::load_memory_prompt_with_options(&subject.cwd, options)
            .filter(|text| !text.is_empty())
            .map(|text| vec![PluginPromptSection::new(SECTION_NAME, Rung::Memory, text)])
            .unwrap_or_default()
    }

    fn injected_files(&self, cwd: &str) -> Vec<InjectedPromptFile> {
        if !crate::memory::settings::is_auto_memory_enabled(std::path::Path::new(cwd)) {
            return Vec::new();
        }
        let budget = crate::memory::prompt::MemoryPromptBudget::default_runtime();
        crate::memory::prompt::load_memory_entrypoints_for_cache_with_budget(cwd, Some(budget))
            .into_iter()
            .map(|snapshot| InjectedPromptFile {
                path: snapshot.path,
                content: snapshot.raw_content,
                is_partial_view: snapshot.content_differs_from_disk,
            })
            .collect()
    }
}
