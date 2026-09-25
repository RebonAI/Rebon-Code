//! Protocol-agnostic session infrastructure.
//!
//! Everything in this crate is about *where a Rebon session lives on disk* and
//! *how it is mutated safely* — it says nothing about which protocol drove the
//! session. ACP, the TUI, the desktop app, and background jobs all share these
//! primitives.
//!
//! - [`session_storage`] — the on-disk JSONL transcript schema and its
//!   accessors: project directory keying
//!   (`${config_home}/projects/${project_dir_component(cwd)}/${sid}.jsonl`,
//!   where [`session_storage::project_dir_component`] is
//!   `sanitize_path(cwd_identity(cwd))` — the `cwd_identity` case fold is
//!   what keeps one Windows cwd from splitting into two project dirs),
//!   `parentUuid` chain reconstruction, append/finalize writes,
//!   single-instance session locks, rewind / summarize receipts, and the
//!   ultraplan run store.
//! - [`file_history`] — per-turn file snapshots taken around `Write`/`Edit`
//!   tool calls, plus the restore preflight and apply path behind `/rewind`.
//! - [`memory_paths`] — the durable-memory directories under the same
//!   config home and the same project key: `<config_home>/memory/user/` and
//!   `<config_home>/projects/<project_dir_component>/memory/`. A layout fact
//!   rather than memory behavior: a caller that only has to recognise these
//!   paths must not have to depend on the memory feature to do it.

pub mod config_home;
pub mod file_history;
pub mod held_lock;
pub mod memory_paths;
pub mod model_selection;
pub mod scratchpad;
pub mod session_storage;

pub use scratchpad::{remove_scratchpad_for, scratchpad_dir_for};

pub use config_home::{config_home_with_env, platform_home_dir, platform_home_with_env};

pub use file_history::{
    FileHistoryApplyError, FileHistoryApplyReport, FileHistoryBackup, FileHistoryDiffStats,
    FileHistoryLoadError, FileHistoryManifest, FileHistorySnapshot, FileHistoryStore,
    FileRestoreCapability, FileRestoreOperation, FileRestorePreflight, FileRestorePreflightPath,
    FileRestoreUnavailableReason,
};

pub use held_lock::HeldSessionLock;

/// The long-path rule behind [`replace_file_atomically`], for any other
/// hand-written Win32 call that has to name a file.
#[cfg(windows)]
pub use session_storage::wide_path_for_win32;
pub use session_storage::{
    append_transcript_entry, classify_transcript_prompt_uuid, cleanup_stale_active_locks,
    copy_session_to_cwd, create_session_on_disk, cwd_identity, default_config_home_dir,
    default_projects_root, ensure_session_file_path, finalize_transcript_entry,
    find_session_transcript_cwd, fold_project_dir_name, format_system_time_iso_ms,
    is_session_active, last_turn_assistant_text, latest_active_run_for_session,
    list_ultraplan_runs, load_agent_session_id, load_raw_transcript_from_file, load_session_agent,
    load_session_created_at_ms, load_session_hidden_from_chats, load_session_history,
    load_session_mode, load_session_plan_entered_from, load_session_title,
    load_transcript_from_file, load_ultraplan_run,
    move_session_to_cwd, parse_transcript_jsonl, project_dir_component, project_dir_path,
    read_project_cwd_sidecar, read_session_owner, reconstruct_chain, record_session_created_at,
    remove_session_owner, rewind_conversation, rewind_conversation_locked, same_cwd, sanitize_path,
    save_agent_session_id, save_session_agent, save_session_hidden_from_chats, save_session_mode,
    save_session_plan_entered_from,
    save_session_title, save_ultraplan_run, save_ultraplan_run_cas, session_active_lock_path,
    session_owner_path, session_transcript_cwds, summarize_conversation_locked,
    transcript_file_path, transcript_first_timestamp_ms, try_acquire_session_active_lock,
    ultraplan_run_dir_path, ultraplan_run_path, update_session_metadata, write_session_owner,
    write_transcript_entries, HistoryTurn, HistoryTurnCompletion, LoadedTranscript,
    RawTranscriptFile, RewindConversationError, RewindConversationReceipt,
    RewindConversationRequest, RunSummary, SessionActiveLock, SessionHistorySnapshot,
    SessionOwnerDescriptor, SessionOwnerSurface, SummarizeConversationMode,
    SummarizeConversationReceipt, SummarizeConversationRequest, TranscriptEntry,
    TranscriptPromptUuidState, TranscriptStamp, TranscriptWriteEntry, UltraplanRunStoreError,
    MAX_SANITIZED_LENGTH, SESSION_AGENT_LOCAL, SESSION_OWNER_SUFFIX, SESSION_OWNER_VERSION,
};
pub use session_storage::{
    replace_file_atomically, write_file_atomically, write_private_file_atomically,
};
