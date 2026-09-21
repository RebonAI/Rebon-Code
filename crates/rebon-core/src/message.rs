//! Rust types for the rebon message / transcript layer.
//!
//! This module covers a focused subset of the message graph. The types
//! here don't live next to the code that constructs them (unlike a
//! scattered-by-callsite layout); the three areas that matter for this
//! module are:
//!
//! * The `Attachment` union, hook attachments, and the plan-mode /
//!   auto-mode / file-reference attachments.
//! * The system API error message and the legacy-attachment allowlist.
//! * Transcript replay logic and the legacy top-level `progress` row
//!   bridge.
//!
//! Every field name and discriminator here is chosen to match the JSON
//! of the transcript (`.jsonl`) rows and the in-memory message stream, so
//! transcripts stay wire-compatible.
//!
//! ## Scope
//!
//! Implemented:
//!
//! * `Message::StreamRequestStart` — transient
//!   `{ "type": "stream_request_start" }` yield. Not
//!   persisted.
//! * `SystemMessage::ApiError` — persisted `api_error` transcript row.
//! * `SystemMessage::FileSnapshot` — persisted `file_snapshot` transcript
//!   row.
//! * `AttachmentMessage` + the nine `hook_*` attachment variants required to
//!   model `HookResultMessage` faithfully. See [`HookResultMessage`] for the
//!   alias-vs-newtype rationale.
//!
//! * `Attachment::CompactFileReference` — carries `filename` and
//!   `display_path`.
//! * `Attachment::PdfReference` — carries `filename`, `page_count`,
//!   `file_size`, and `display_path`.
//! * `Attachment::AgentMention` — carries only `agent_type`.
//! * `Attachment::EditedTextFile` — carries `filename` and `snippet`.
//! * `Attachment::PlanMode` — carries `reminder_type`, the optional
//!   `is_sub_agent`, `plan_file_path`, and `plan_exists`.
//! * `Attachment::PlanModeReentry` — carries only `plan_file_path`.
//! * `Attachment::PlanModeExit` — carries `plan_file_path` and `plan_exists`.
//! * `Attachment::AutoMode` — carries only `reminder_type`.
//! * `Attachment::AutoModeExit` — unit
//!   variant that serializes as `{ "type": "auto_mode_exit" }`.
//! * Open-world `Other(serde_json::Value)` fallback on [`Message`],
//!   [`SystemMessage`], and [`Attachment`]. Justified by real transcript
//!   churn: five removed attachment types
//!   (`autocheckpointing`, `background_task_status`, `todo`,
//!   `task_progress`, `ultramemory`) still appear in old transcripts, as
//!   does a removed top-level `progress` row. Rust must preserve unknown
//!   rows instead of failing to parse them, so deserialization is
//!   open-world:
//!   any row whose discriminator is unknown, or whose known discriminator
//!   has malformed fields, falls through to `Other(JsonValue)` and
//!   round-trips byte-for-byte via the preserved raw value.
//!
//! * `Attachment::Directory` — carries `path`, `content`, and `display_path`.
//! * `Attachment::SelectedLinesInIde` — carries `ide_name`, `line_start`,
//!   `line_end`, `filename`, `content`, and `display_path`.
//! * `Attachment::OpenedFileInIde` — carries only `filename`.
//!
//! These three are each a flat record of primitives (`String` plus, in the
//! case of `selected_lines_in_ide`, two line-number fields), with no
//! dependency on the much heavier `FileReadToolOutput` payload that gates
//! `file`, `already_read_file`, and `edited_image_file`. Modeling them as
//! typed variants (instead of leaving them in [`Attachment::Other`])
//! eliminates three of the most common attachment kinds from the
//! open-world fallback; the `FileReadToolOutput`-bearing variants remain
//! in the fallback until they get dedicated fixtures.
//!
//! Not yet implemented (still land in the open-world fallback):
//!
//! * The remaining non-hook attachment variants — `file`,
//!   `already_read_file`, `edited_image_file`, `todo_reminder`,
//!   `task_reminder`, `nested_memory`, `relevant_memories`, `dynamic_skill`,
//!   `skill_listing`, `skill_discovery`, `queued_command`, `output_style`,
//!   `diagnostics`, `critical_system_reminder`, `plan_file_reference`,
//!   `mcp_resource`, `empty_reminder`, token/budget variants, team_*,
//!   memory variants, …
//! * `SystemThinkingMessage` and other system subtypes beyond
//!   [`SystemMessage::ApiError`] / [`SystemMessage::FileSnapshot`] — no
//!   live constructor exists in the current tree and the type is not
//!   persisted. Unknown `subtype` values land in [`SystemMessage::Other`]
//!   via the open-world fallback.
//! * User / assistant / normalized / progress / tombstone top-level
//!   message variants and the full top-level `Message` union. Unknown
//!   top-level `type` values (including the legacy `progress` row) land
//!   in [`Message::Other`] via the open-world fallback.
//!
//! ## Serde strategy
//!
//! The three open-world enums ([`Message`], [`SystemMessage`], [`Attachment`])
//! all follow the same pattern: the typed variants dispatch through a private
//! helper enum that derives `Serialize` / `Deserialize` with the usual
//! `#[serde(tag = "type", rename_all = "snake_case")]` (or `tag = "subtype"`
//! for `SystemMessage`). The outer enums implement `Serialize` and
//! `Deserialize` manually:
//!
//! * Serialize dispatches each typed variant through the private helper
//!   (borrowing the inner payload, so typed-variant serialization is
//!   allocation-free); the `Other` variant serializes its raw
//!   [`serde_json::Value`] directly.
//! * Deserialize buffers input into a [`serde_json::Value`], tries the
//!   helper first, and on any error (unknown discriminator, missing
//!   field, wrong literal, malformed payload) stores the raw `Value` as
//!   `Other`. This is the open-world fallback.
//!
//! Because the helpers use serde's internal tagging, nesting them yields
//! the wire shape `{ "type": "system", "subtype": "api_error", ... }`,
//! matching the shape transcripts use for `system` / `api_error` rows.
//!
//! Field renaming follows the transcript's camelCase convention
//! (`retryInMs`, `snapshotFiles`, `isMeta`, `displayPath`, `pageCount`,
//! `fileSize`, `agentType`, `reminderType`, `isSubAgent`, `planFilePath`,
//! `planExists`, …). One quirk: `toolUseID` keeps the uppercase `ID`
//! suffix — the Rust fields use an explicit
//! `#[serde(rename = "toolUseID")]` override, not bulk camelCase.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Severity level for `system` subtype messages. Matches the transcript's
/// `SystemMessageLevel`: one of `"info"`, `"warning"`, `"error"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemMessageLevel {
    Info,
    Warning,
    Error,
}

/// Top-level discriminated message union.
///
/// Typed variants dispatch through a private internally-tagged helper.
/// Unknown top-level `type` values (e.g. the legacy `progress` row), or rows
/// whose known `type` has missing / wrong-shaped fields, fall through to
/// [`Message::Other`] and round-trip as raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// `{ "type": "stream_request_start" }` — transient event. Carries no
    /// uuid/timestamp. Not persisted.
    StreamRequestStart,

    /// All `"type": "system"` rows. Further discriminated by `subtype`.
    System(SystemMessage),

    /// `"type": "attachment"` rows. The attachment payload carries its own
    /// inner `type` discriminator.
    Attachment(AttachmentMessage),

    /// Open-world fallback for unknown / malformed top-level rows. The raw
    /// JSON value is preserved unchanged so the round-trip is lossless.
    Other(JsonValue),
}

impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Borrowed helper used for the typed variants. `Other` short-circuits
        // to a direct pass-through of the raw Value so legacy rows survive a
        // round trip unchanged.
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Repr<'a> {
            StreamRequestStart,
            System(&'a SystemMessage),
            Attachment(&'a AttachmentMessage),
        }

        match self {
            Self::StreamRequestStart => Repr::StreamRequestStart.serialize(serializer),
            Self::System(s) => Repr::System(s).serialize(serializer),
            Self::Attachment(a) => Repr::Attachment(a).serialize(serializer),
            Self::Other(v) => v.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Buffer into a Value so we can always preserve the raw row if the
        // typed dispatch bails out. The fallback is unconditional — any
        // deserialization error (unknown discriminator, missing required
        // field, wrong literal, …) lands in `Other`.
        let value = JsonValue::deserialize(deserializer)?;

        let typed = value.get("type").and_then(|t| t.as_str()).and_then(|ty| {
            match ty {
                "stream_request_start" => Some(Self::StreamRequestStart),
                "system" => {
                    // `SystemMessage` has its own open-world Deserialize, so
                    // this only fails if the outer shape is not an object.
                    serde_json::from_value::<SystemMessage>(value.clone())
                        .ok()
                        .map(Self::System)
                }
                "attachment" => serde_json::from_value::<AttachmentMessage>(value.clone())
                    .ok()
                    .map(Self::Attachment),
                _ => None,
            }
        });

        Ok(typed.unwrap_or(Self::Other(value)))
    }
}

/// Transient alias for `Message::StreamRequestStart` — named to match the
/// transcript's `RequestStartEvent` type. Callers that want a standalone
/// value without the `Message` wrapper can use [`request_start_event_json`].
pub type RequestStartEvent = Message;

/// Construct the canonical JSON value for a `stream_request_start` event
/// (`{ "type": "stream_request_start" }`).
pub fn request_start_event_json() -> JsonValue {
    serde_json::json!({ "type": "stream_request_start" })
}

// ---------------------------------------------------------------------------
// system/*
// ---------------------------------------------------------------------------

/// All `"type": "system"` messages modelled so far. Discriminated by
/// `subtype`. Unknown subtypes fall through to [`SystemMessage::Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum SystemMessage {
    /// `{ "type": "system", "subtype": "api_error", ... }` row; payload is
    /// [`SystemApiErrorPayload`].
    ApiError(SystemApiErrorPayload),

    /// `{ "type": "system", "subtype": "file_snapshot", ... }` row; payload is
    /// [`SystemFileSnapshotPayload`].
    FileSnapshot(SystemFileSnapshotPayload),

    /// Open-world fallback. Catches e.g. `"subtype": "thinking"` and any
    /// future subtype not yet modelled. The raw row (including the outer
    /// `"type": "system"` key) is preserved unchanged for lossless round-trip.
    Other(JsonValue),
}

impl Serialize for SystemMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(tag = "subtype", rename_all = "snake_case")]
        enum Repr<'a> {
            ApiError(&'a SystemApiErrorPayload),
            FileSnapshot(&'a SystemFileSnapshotPayload),
        }

        match self {
            Self::ApiError(p) => Repr::ApiError(p).serialize(serializer),
            Self::FileSnapshot(p) => Repr::FileSnapshot(p).serialize(serializer),
            Self::Other(v) => v.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for SystemMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "subtype", rename_all = "snake_case")]
        enum Repr {
            ApiError(SystemApiErrorPayload),
            FileSnapshot(SystemFileSnapshotPayload),
        }

        let value = JsonValue::deserialize(deserializer)?;
        match serde_json::from_value::<Repr>(value.clone()) {
            Ok(Repr::ApiError(p)) => Ok(Self::ApiError(p)),
            Ok(Repr::FileSnapshot(p)) => Ok(Self::FileSnapshot(p)),
            Err(_) => Ok(Self::Other(value)),
        }
    }
}

/// Payload body for [`SystemMessage::ApiError`].
///
/// ## Why `error` and `cause` are `JsonValue`
///
/// In the wire format, `error` is a serialized provider error object and
/// `cause`, when present, is a serialized error object too. Both carry
/// whatever properties the error had when the row was logged. The
/// resulting shape is irregular — different error kinds emit different
/// keys — and callers only need to read it as opaque context. The Rust side
/// stays honest by storing raw [`serde_json::Value`] and can narrow the
/// shape later if it ever stabilises.
///
/// ## Key order vs insertion order
///
/// Keys are inserted in this order:
/// `type, subtype, level, cause, error, retryInMs, retryAttempt, maxRetries,
/// timestamp, uuid`. Rust field order below matches that so
/// `to_string_pretty` output stays intuitive when comparing against
/// fixtures, but note that JSON itself is key-order independent and the
/// round-trip tests below do not rely on ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemApiErrorPayload {
    pub level: SystemMessageLevel,

    /// Optional — present only when the error carried a cause. An absent
    /// cause has no key on the wire, so it is skipped on serialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<JsonValue>,

    /// The raw error payload as it landed in the transcript. Shape is
    /// deliberately unspecified; see the type-level doc comment.
    pub error: JsonValue,

    pub retry_in_ms: u64,
    pub retry_attempt: u32,
    pub max_retries: u32,
    pub timestamp: String,
    pub uuid: String,
}

impl SystemApiErrorPayload {
    /// Convenience constructor for a `system` / `api_error` payload.
    /// `level` is always `Error`.
    pub fn new(
        error: JsonValue,
        retry_in_ms: u64,
        retry_attempt: u32,
        max_retries: u32,
        timestamp: impl Into<String>,
        uuid: impl Into<String>,
    ) -> Self {
        Self {
            level: SystemMessageLevel::Error,
            cause: None,
            error,
            retry_in_ms,
            retry_attempt,
            max_retries,
            timestamp: timestamp.into(),
            uuid: uuid.into(),
        }
    }
}

/// Convenience alias — the full `"type": "system", "subtype": "api_error"` row.
pub type SystemApiErrorMessage = SystemApiErrorPayload;

/// A single entry inside `snapshotFiles`. Fields are plain lowercase,
/// no rename needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSnapshotEntry {
    pub key: String,
    pub path: String,
    pub content: String,
}

/// Payload body for [`SystemMessage::FileSnapshot`].
///
/// The row is always written with `"content": "File snapshot"`,
/// `"level": "info"`, and `"isMeta": true` (see
/// [`SystemFileSnapshotPayload::new`]). Rust keeps them as real fields (rather than constants) so round-tripping a
/// transcript row that was mutated by a later code path stays lossless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemFileSnapshotPayload {
    /// Always `"File snapshot"` at the construction site. Stored as-is.
    pub content: String,
    /// Always `SystemMessageLevel::Info` at the construction site.
    pub level: SystemMessageLevel,
    /// Always `true` at the construction site.
    pub is_meta: bool,
    pub timestamp: String,
    pub uuid: String,
    pub snapshot_files: Vec<FileSnapshotEntry>,
}

impl SystemFileSnapshotPayload {
    /// Convenience constructor for the `file_snapshot` payload, fixing the
    /// fields the row is always written with.
    pub fn new(
        timestamp: impl Into<String>,
        uuid: impl Into<String>,
        snapshot_files: Vec<FileSnapshotEntry>,
    ) -> Self {
        Self {
            content: "File snapshot".to_string(),
            level: SystemMessageLevel::Info,
            is_meta: true,
            timestamp: timestamp.into(),
            uuid: uuid.into(),
            snapshot_files,
        }
    }
}

pub type SystemFileSnapshotMessage = SystemFileSnapshotPayload;

// ---------------------------------------------------------------------------
// attachment/*
// ---------------------------------------------------------------------------

/// `"type": "attachment"` payload body.
///
/// `attachment` is listed first so `to_string_pretty` output matches the
/// established key order; wire compatibility does not depend on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachmentMessage {
    pub attachment: Attachment,
    pub uuid: String,
    pub timestamp: String,
}

/// `HookResultMessage` — there is no dedicated factory or nominal type for
/// this. The name refers to an [`AttachmentMessage`] whose `attachment`
/// field happens to be one of the `hook_*` variants. It flows anywhere a
/// message list does.
///
/// **Choice**: alias, not newtype. Rationale:
///
/// 1. **Wire faithfulness** — a Rust newtype would introduce a nominal
///    distinction that doesn't exist at runtime on the wire. Any
///    serialization/deserialization would round-trip through that newtype
///    only by coincidence; the nominal layer would add zero wire safety.
/// 2. **No structural constraint to enforce** — Rust's type system can't
///    meaningfully narrow `attachment: Attachment` to "only hook variants"
///    without either a sealed trait (overkill here) or a duplicate
///    `HookAttachment`-typed sibling struct (wastes the generic `Attachment`
///    enum we already need). A newtype wrapping the same field doesn't
///    narrow anything.
/// 3. **Downstream pattern matching** — callers that truly care whether
///    an attachment is a hook variant are already going to `match` on
///    [`Attachment`]; a helper predicate ([`Attachment::is_hook_variant`])
///    is a lighter surface than a whole type.
///
/// If a real nominal distinction is ever needed (e.g. for
/// `HookResultMessage`-only APIs), the cleanest path is to split the hook
/// variants into a dedicated `HookAttachment` enum and parameterize
/// `AttachmentMessage` over the attachment type. That's deferred.
pub type HookResultMessage = AttachmentMessage;

/// The `attachment` inner union.
///
/// This covers the nine `hook_*` variants plus a small set of
/// file-reference / plan-mode / auto-mode variants. Everything else
/// (including the removed legacy attachment types) lands in
/// [`Attachment::Other`], which preserves the raw JSON unchanged.
#[derive(Debug, Clone, PartialEq)]
pub enum Attachment {
    // ----- the nine hook_* variants -----
    HookCancelled(HookCancelledAttachment),
    HookBlockingError(HookBlockingErrorAttachment),
    HookNonBlockingError(HookNonBlockingErrorAttachment),
    HookErrorDuringExecution(HookErrorDuringExecutionAttachment),
    HookStoppedContinuation(HookStoppedContinuationAttachment),
    HookSuccess(HookSuccessAttachment),
    HookAdditionalContext(HookAdditionalContextAttachment),
    HookSystemMessage(HookSystemMessageAttachment),
    HookPermissionDecision(HookPermissionDecisionAttachment),

    // ----- file-reference / plan-mode / auto-mode variants -----
    CompactFileReference(CompactFileReferenceAttachment),
    PdfReference(PdfReferenceAttachment),
    AgentMention(AgentMentionAttachment),
    EditedTextFile(EditedTextFileAttachment),
    PlanMode(PlanModeAttachment),
    PlanModeReentry(PlanModeReentryAttachment),
    PlanModeExit(PlanModeExitAttachment),
    AutoMode(AutoModeAttachment),
    /// Unit variant — serializes as `{ "type": "auto_mode_exit" }`.
    /// Matches the empty inline variant.
    AutoModeExit,

    // ----- directory + IDE file-reference variants -----
    Directory(DirectoryAttachment),
    SelectedLinesInIde(SelectedLinesInIdeAttachment),
    OpenedFileInIde(OpenedFileInIdeAttachment),

    /// Open-world fallback for unknown / legacy / malformed attachment
    /// rows. Preserved as raw JSON so lossless round-trip is guaranteed.
    Other(JsonValue),
}

impl Serialize for Attachment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Repr<'a> {
            HookCancelled(&'a HookCancelledAttachment),
            HookBlockingError(&'a HookBlockingErrorAttachment),
            HookNonBlockingError(&'a HookNonBlockingErrorAttachment),
            HookErrorDuringExecution(&'a HookErrorDuringExecutionAttachment),
            HookStoppedContinuation(&'a HookStoppedContinuationAttachment),
            HookSuccess(&'a HookSuccessAttachment),
            HookAdditionalContext(&'a HookAdditionalContextAttachment),
            HookSystemMessage(&'a HookSystemMessageAttachment),
            HookPermissionDecision(&'a HookPermissionDecisionAttachment),
            CompactFileReference(&'a CompactFileReferenceAttachment),
            PdfReference(&'a PdfReferenceAttachment),
            AgentMention(&'a AgentMentionAttachment),
            EditedTextFile(&'a EditedTextFileAttachment),
            PlanMode(&'a PlanModeAttachment),
            PlanModeReentry(&'a PlanModeReentryAttachment),
            PlanModeExit(&'a PlanModeExitAttachment),
            AutoMode(&'a AutoModeAttachment),
            AutoModeExit,
            Directory(&'a DirectoryAttachment),
            SelectedLinesInIde(&'a SelectedLinesInIdeAttachment),
            OpenedFileInIde(&'a OpenedFileInIdeAttachment),
        }

        match self {
            Self::HookCancelled(v) => Repr::HookCancelled(v).serialize(serializer),
            Self::HookBlockingError(v) => Repr::HookBlockingError(v).serialize(serializer),
            Self::HookNonBlockingError(v) => Repr::HookNonBlockingError(v).serialize(serializer),
            Self::HookErrorDuringExecution(v) => {
                Repr::HookErrorDuringExecution(v).serialize(serializer)
            }
            Self::HookStoppedContinuation(v) => {
                Repr::HookStoppedContinuation(v).serialize(serializer)
            }
            Self::HookSuccess(v) => Repr::HookSuccess(v).serialize(serializer),
            Self::HookAdditionalContext(v) => Repr::HookAdditionalContext(v).serialize(serializer),
            Self::HookSystemMessage(v) => Repr::HookSystemMessage(v).serialize(serializer),
            Self::HookPermissionDecision(v) => {
                Repr::HookPermissionDecision(v).serialize(serializer)
            }
            Self::CompactFileReference(v) => Repr::CompactFileReference(v).serialize(serializer),
            Self::PdfReference(v) => Repr::PdfReference(v).serialize(serializer),
            Self::AgentMention(v) => Repr::AgentMention(v).serialize(serializer),
            Self::EditedTextFile(v) => Repr::EditedTextFile(v).serialize(serializer),
            Self::PlanMode(v) => Repr::PlanMode(v).serialize(serializer),
            Self::PlanModeReentry(v) => Repr::PlanModeReentry(v).serialize(serializer),
            Self::PlanModeExit(v) => Repr::PlanModeExit(v).serialize(serializer),
            Self::AutoMode(v) => Repr::AutoMode(v).serialize(serializer),
            Self::AutoModeExit => Repr::AutoModeExit.serialize(serializer),
            Self::Directory(v) => Repr::Directory(v).serialize(serializer),
            Self::SelectedLinesInIde(v) => Repr::SelectedLinesInIde(v).serialize(serializer),
            Self::OpenedFileInIde(v) => Repr::OpenedFileInIde(v).serialize(serializer),
            Self::Other(v) => v.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Attachment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Repr {
            HookCancelled(HookCancelledAttachment),
            HookBlockingError(HookBlockingErrorAttachment),
            HookNonBlockingError(HookNonBlockingErrorAttachment),
            HookErrorDuringExecution(HookErrorDuringExecutionAttachment),
            HookStoppedContinuation(HookStoppedContinuationAttachment),
            HookSuccess(HookSuccessAttachment),
            HookAdditionalContext(HookAdditionalContextAttachment),
            HookSystemMessage(HookSystemMessageAttachment),
            HookPermissionDecision(HookPermissionDecisionAttachment),
            CompactFileReference(CompactFileReferenceAttachment),
            PdfReference(PdfReferenceAttachment),
            AgentMention(AgentMentionAttachment),
            EditedTextFile(EditedTextFileAttachment),
            PlanMode(PlanModeAttachment),
            PlanModeReentry(PlanModeReentryAttachment),
            PlanModeExit(PlanModeExitAttachment),
            AutoMode(AutoModeAttachment),
            AutoModeExit,
            Directory(DirectoryAttachment),
            SelectedLinesInIde(SelectedLinesInIdeAttachment),
            OpenedFileInIde(OpenedFileInIdeAttachment),
        }

        let value = JsonValue::deserialize(deserializer)?;
        match serde_json::from_value::<Repr>(value.clone()) {
            Ok(repr) => Ok(match repr {
                Repr::HookCancelled(v) => Self::HookCancelled(v),
                Repr::HookBlockingError(v) => Self::HookBlockingError(v),
                Repr::HookNonBlockingError(v) => Self::HookNonBlockingError(v),
                Repr::HookErrorDuringExecution(v) => Self::HookErrorDuringExecution(v),
                Repr::HookStoppedContinuation(v) => Self::HookStoppedContinuation(v),
                Repr::HookSuccess(v) => Self::HookSuccess(v),
                Repr::HookAdditionalContext(v) => Self::HookAdditionalContext(v),
                Repr::HookSystemMessage(v) => Self::HookSystemMessage(v),
                Repr::HookPermissionDecision(v) => Self::HookPermissionDecision(v),
                Repr::CompactFileReference(v) => Self::CompactFileReference(v),
                Repr::PdfReference(v) => Self::PdfReference(v),
                Repr::AgentMention(v) => Self::AgentMention(v),
                Repr::EditedTextFile(v) => Self::EditedTextFile(v),
                Repr::PlanMode(v) => Self::PlanMode(v),
                Repr::PlanModeReentry(v) => Self::PlanModeReentry(v),
                Repr::PlanModeExit(v) => Self::PlanModeExit(v),
                Repr::AutoMode(v) => Self::AutoMode(v),
                Repr::AutoModeExit => Self::AutoModeExit,
                Repr::Directory(v) => Self::Directory(v),
                Repr::SelectedLinesInIde(v) => Self::SelectedLinesInIde(v),
                Repr::OpenedFileInIde(v) => Self::OpenedFileInIde(v),
            }),
            Err(_) => Ok(Self::Other(value)),
        }
    }
}

impl Attachment {
    /// Predicate used by callers that need to gate on "is this the
    /// attachment an `HookResultMessage` would carry?" without introducing
    /// a nominal wrapper. See [`HookResultMessage`] for the rationale.
    pub fn is_hook_variant(&self) -> bool {
        matches!(
            self,
            Attachment::HookCancelled(_)
                | Attachment::HookBlockingError(_)
                | Attachment::HookNonBlockingError(_)
                | Attachment::HookErrorDuringExecution(_)
                | Attachment::HookStoppedContinuation(_)
                | Attachment::HookSuccess(_)
                | Attachment::HookAdditionalContext(_)
                | Attachment::HookSystemMessage(_)
                | Attachment::HookPermissionDecision(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookBlockingError {
    #[serde(rename = "blockingError")]
    pub blocking_error: String,
    pub command: String,
}

/// Hook lifecycle event name. Matches the wire `HookEvent` union.
/// Variant names use the exact PascalCase strings the wire format emits — no
/// serde rename needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    PermissionRequest,
    PermissionDenied,
    Setup,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
    Elicitation,
    ElicitationResult,
    ConfigChange,
    WorktreeCreate,
    WorktreeRemove,
    InstructionsLoaded,
    CwdChanged,
    FileChanged,
}

// ---- hook_* variant payloads ----------------------------------------------
//
// Each struct matches a wire type exactly.
// Optional fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`
// so an absent value has no key in the wire form.
//
// NOTE: the wire format uses `toolUseID` (uppercase `ID`), not `toolUseId`. This breaks
// the `rename_all = "camelCase"` heuristic so every hook payload uses an
// explicit `#[serde(rename = "toolUseID")]` override.

/// `hook_cancelled` — the hook run was cancelled. Carries the hook name, the
/// tool use ID and lifecycle event, plus the optional command and duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookCancelledAttachment {
    pub hook_name: String,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// `hook_blocking_error` — the hook returned a blocking error. Carries the
/// [`HookBlockingError`] (its message and the command that raised it), the hook
/// name, the tool use ID, and the lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookBlockingErrorAttachment {
    pub blocking_error: HookBlockingError,
    pub hook_name: String,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
}

/// `hook_non_blocking_error` — the hook failed without blocking the tool call.
/// Carries the hook name, the captured `stdout` / `stderr` and the exit code,
/// the tool use ID and lifecycle event, plus the optional command and duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookNonBlockingErrorAttachment {
    pub hook_name: String,
    pub stderr: String,
    pub stdout: String,
    pub exit_code: i32,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// `hook_error_during_execution` — the hook itself failed while running.
/// Carries the rendered error `content`, the hook name, the tool use ID and
/// lifecycle event, plus the optional command and duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookErrorDuringExecutionAttachment {
    pub content: String,
    pub hook_name: String,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// `hook_stopped_continuation` — the hook stopped the turn from continuing.
/// Carries the explanatory `message`, the hook name, the tool use ID, and the
/// lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStoppedContinuationAttachment {
    pub message: String,
    pub hook_name: String,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
}

/// `hook_success` — the hook ran and succeeded. Carries the rendered output
/// `content`, the hook name, the tool use ID and lifecycle event, plus the
/// optional `stdout` / `stderr` / exit code, command, and duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSuccessAttachment {
    pub content: String,
    pub hook_name: String,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// `hook_additional_context` — extra context the hook contributed. Carries
/// `content` (a list of context strings), the hook name, the tool use ID, and
/// the lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookAdditionalContextAttachment {
    pub content: Vec<String>,
    pub hook_name: String,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
}

/// `hook_system_message` — a system-role message produced by the hook. Carries
/// its `content`, the hook name, the tool use ID, and the lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSystemMessageAttachment {
    pub content: String,
    pub hook_name: String,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
}

/// `hook_permission_decision` — the hook's verdict on a permission request.
/// Carries the `decision` ([`HookPermissionDecision`]), the tool use ID, and the
/// lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookPermissionDecisionAttachment {
    pub decision: HookPermissionDecision,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub hook_event: HookEvent,
}

/// `"allow"` or `"deny"` — used by [`HookPermissionDecisionAttachment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookPermissionDecision {
    Allow,
    Deny,
}

// ---- file-reference / plan-mode / auto-mode payloads -----------------------

/// The `"full"` / `"sparse"` literal shared by [`PlanModeAttachment`] and
/// [`AutoModeAttachment`]. Unknown string literals fail to deserialize,
/// which — via [`Attachment`]'s open-world Deserialize — routes the whole
/// row into [`Attachment::Other`] instead of erroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReminderType {
    Full,
    Sparse,
}

/// `compact_file_reference` — a file reference carried as `filename` plus the
/// `display_path` used to render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactFileReferenceAttachment {
    pub filename: String,
    pub display_path: String,
}

/// `pdf_reference` — a referenced PDF: `filename`, `page_count`, `file_size`,
/// and `display_path`.
///
/// `pageCount` and `fileSize` are `u64` rather than `u32` because the wire
/// format uses plain `number` and real PDF fixtures can have file sizes
/// above `i32::MAX` (a 2 GiB+ PDF is rare but not impossible).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfReferenceAttachment {
    pub filename: String,
    pub page_count: u64,
    pub file_size: u64,
    pub display_path: String,
}

/// `agent_mention` — an @-mention of a subagent, identified solely by its
/// `agent_type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMentionAttachment {
    pub agent_type: String,
}

/// `edited_text_file` — an edited text file, carried as its `filename` and the
/// edited `snippet`.
///
/// Both fields are single-word lowercase on the wire, so no `rename_all`
/// is required. Embedded newlines in `snippet` are preserved by
/// `serde_json` as `"\n"` escape sequences on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditedTextFileAttachment {
    pub filename: String,
    pub snippet: String,
}

/// `plan_mode` — the plan-mode reminder: `reminder_type`, the optional
/// `is_sub_agent` flag, `plan_file_path`, and `plan_exists`.
///
/// `is_sub_agent` is `Option<bool>` so that:
///
/// * an omitted `isSubAgent` on the wire round-trips as `None`
///   (`skip_serializing_if = "Option::is_none"`), and
/// * an explicit `isSubAgent: false` round-trips as `Some(false)` rather
///   than being silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeAttachment {
    pub reminder_type: ReminderType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_sub_agent: Option<bool>,
    pub plan_file_path: String,
    pub plan_exists: bool,
}

/// `plan_mode_reentry` — re-entering plan mode. Carries just the
/// `plan_file_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeReentryAttachment {
    pub plan_file_path: String,
}

/// `plan_mode_exit` — leaving plan mode. Carries the `plan_file_path` and
/// whether the plan file still exists (`plan_exists`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeExitAttachment {
    pub plan_file_path: String,
    pub plan_exists: bool,
}

/// `auto_mode` — the auto-mode reminder. Carries just the `reminder_type`
/// ([`ReminderType`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoModeAttachment {
    pub reminder_type: ReminderType,
}

// ---- directory + IDE file-reference payloads -------------------------------
//
// Three flat-record file/directory references promoted out of the open-world
// `Attachment::Other` fallback. Each one matches a specific inline variant;
// field names follow the same camelCase
// convention as the file-reference / plan-mode / auto-mode payloads —
// `displayPath`, `lineStart`, `lineEnd`, `ideName`.

/// `directory` — a directory reference: the `path`, the rendered `content`, and
/// the `display_path`.
///
/// Carries a `path` (the absolute or workspace-relative path the user
/// at-mentioned), `content` (the rendered directory listing string), and a
/// `displayPath` (path relative to the cwd at attachment-creation time,
/// used for stable rendering across cwd changes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryAttachment {
    pub path: String,
    pub content: String,
    pub display_path: String,
}

/// `selected_lines_in_ide` — an IDE line selection: `ide_name`, `line_start`,
/// `line_end`, `filename`, `content`, and `display_path`.
///
/// `line_start` and `line_end` use `u64` (not `u32`) for the same reason
/// `PdfReferenceAttachment.page_count` does: the wire's plain `number` is
/// f64-backed and the wire shape carries no upper bound, so the safe Rust
/// implementation matches the documented "above-`i32::MAX` is legal on the
/// wire" rule rather than silently truncating large line numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedLinesInIdeAttachment {
    pub ide_name: String,
    pub line_start: u64,
    pub line_end: u64,
    pub filename: String,
    pub content: String,
    pub display_path: String,
}

/// `opened_file_in_ide` — the file opened in the IDE, carried as a single
/// `filename`.
///
/// The simplest of the three directory + IDE file-reference variants — a
/// single `filename` field
/// pointing at the file the IDE opened. The row carries no
/// additional context because the IDE-side hook only provides the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenedFileInIdeAttachment {
    pub filename: String,
}

// ---------------------------------------------------------------------------
// Deferred
// ---------------------------------------------------------------------------

// TODO: the unmodelled top-level message variants (user / assistant /
// normalized / progress / tombstone), the remaining non-hook `Attachment`
// variants listed in the module doc comment (notably the
// `FileReadToolOutput`-bearing `file`, `already_read_file`, and
// `edited_image_file` attachment variants, which need a dedicated
// `FileReadToolOutput` implementation first), and any future
// `SystemMessage` subtype that picks up a live constructor. Until then the
// open-world `Other(JsonValue)` fallback on each enum absorbs them without
// losing wire fidelity.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // System rows + the nine hook_* variants — kept green unchanged.
    // -----------------------------------------------------------------------

    /// Live-ish `api_error` row fixture: a typical provider API error as
    /// it lands in a transcript — status / message / headers fields on
    /// the error, no `cause`. The exact shape of the
    /// `error` payload is treated as opaque (see `SystemApiErrorPayload`
    /// doc comment), which is the whole reason it's a `JsonValue`.
    fn api_error_fixture() -> JsonValue {
        json!({
            "type": "system",
            "subtype": "api_error",
            "level": "error",
            "error": {
                "status": 529,
                "message": "overloaded",
                "headers": { "retry-after": "5" }
            },
            "retryInMs": 5000,
            "retryAttempt": 2,
            "maxRetries": 5,
            "timestamp": "2026-04-07T12:34:56.000Z",
            "uuid": "11111111-2222-3333-4444-555555555555"
        })
    }

    #[test]
    fn api_error_fixture_round_trips() {
        // Deserialize the fixture into the top-level Message enum to prove
        // both the outer `type` tag AND the inner `subtype` tag flatten
        // correctly for a nested internally-tagged enum.
        let fixture = api_error_fixture();
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();

        match &msg {
            Message::System(SystemMessage::ApiError(payload)) => {
                assert_eq!(payload.level, SystemMessageLevel::Error);
                assert!(payload.cause.is_none());
                assert_eq!(payload.retry_in_ms, 5000);
                assert_eq!(payload.retry_attempt, 2);
                assert_eq!(payload.max_retries, 5);
                assert_eq!(payload.timestamp, "2026-04-07T12:34:56.000Z");
                assert_eq!(payload.uuid, "11111111-2222-3333-4444-555555555555");
                // The `error` payload shape is intentionally unspecified;
                // assert we preserved it as opaque JSON.
                assert_eq!(payload.error["status"], 529);
                assert_eq!(payload.error["headers"]["retry-after"], "5");
            }
            other => panic!("expected Message::System(ApiError), got {other:?}"),
        }

        // Round-trip back to JSON and compare semantically. Serde JSON
        // Value equality is key-order independent.
        let reserialized = serde_json::to_value(&msg).unwrap();
        assert_eq!(reserialized, fixture);
    }

    #[test]
    fn api_error_cause_is_omitted_when_none() {
        // A row whose error had no cause carries no `cause` key on the
        // wire. The Rust serializer must do the same.
        let payload = SystemApiErrorPayload::new(
            json!({ "status": 500, "message": "boom" }),
            1000,
            0,
            3,
            "2026-04-07T00:00:00.000Z",
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
        );
        let msg = Message::System(SystemMessage::ApiError(payload));
        let value = serde_json::to_value(&msg).unwrap();
        let obj = value.as_object().unwrap();
        assert!(
            !obj.contains_key("cause"),
            "cause should be omitted when None"
        );
        assert_eq!(obj["type"], "system");
        assert_eq!(obj["subtype"], "api_error");
        assert_eq!(obj["level"], "error");
    }

    #[test]
    fn api_error_cause_round_trips_when_present() {
        let mut payload = SystemApiErrorPayload::new(
            json!({ "status": 503 }),
            500,
            1,
            3,
            "2026-04-07T00:00:00.000Z",
            "dddddddd-dddd-dddd-dddd-dddddddddddd",
        );
        payload.cause = Some(json!({ "name": "TypeError", "message": "nested" }));

        let msg = Message::System(SystemMessage::ApiError(payload));
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["cause"]["name"], "TypeError");

        let back: Message = serde_json::from_value(value).unwrap();
        if let Message::System(SystemMessage::ApiError(p)) = back {
            assert_eq!(p.cause.as_ref().unwrap()["message"], json!("nested"));
        } else {
            panic!("lost variant on round trip");
        }
    }

    #[test]
    fn stream_request_start_serializes_flat() {
        let msg = Message::StreamRequestStart;
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value, json!({ "type": "stream_request_start" }));

        // Round-trip
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back, Message::StreamRequestStart);

        // Helper produces the same shape.
        assert_eq!(
            request_start_event_json(),
            json!({ "type": "stream_request_start" })
        );
    }

    #[test]
    fn file_snapshot_has_expected_wire_shape() {
        let msg = Message::System(SystemMessage::FileSnapshot(SystemFileSnapshotPayload::new(
            "2026-04-07T08:00:00.000Z",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            vec![
                FileSnapshotEntry {
                    key: "plan".to_string(),
                    path: "/tmp/plan.md".to_string(),
                    content: "# plan".to_string(),
                },
                FileSnapshotEntry {
                    key: "todos".to_string(),
                    path: "/tmp/todos.json".to_string(),
                    content: "[]".to_string(),
                },
            ],
        )));

        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["type"], "system");
        assert_eq!(value["subtype"], "file_snapshot");
        assert_eq!(value["content"], "File snapshot");
        assert_eq!(value["level"], "info");
        assert_eq!(value["isMeta"], true);
        assert_eq!(value["snapshotFiles"][0]["key"], "plan");
        assert_eq!(value["snapshotFiles"][0]["path"], "/tmp/plan.md");
        assert_eq!(value["snapshotFiles"][1]["content"], "[]");

        // Round-trip.
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn attachment_message_hook_success_round_trips() {
        let msg = Message::Attachment(AttachmentMessage {
            attachment: Attachment::HookSuccess(HookSuccessAttachment {
                content: "ok".to_string(),
                hook_name: "user-script".to_string(),
                tool_use_id: "toolu_01abc".to_string(),
                hook_event: HookEvent::PreToolUse,
                stdout: Some("stdout output".to_string()),
                stderr: None,
                exit_code: Some(0),
                command: Some("bash ./hook.sh".to_string()),
                duration_ms: Some(42),
            }),
            uuid: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
            timestamp: "2026-04-07T09:00:00.000Z".to_string(),
        });

        let value = serde_json::to_value(&msg).unwrap();

        // Outer shape.
        assert_eq!(value["type"], "attachment");
        assert_eq!(value["uuid"], "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        assert_eq!(value["timestamp"], "2026-04-07T09:00:00.000Z");

        // Inner attachment shape. Note the `toolUseID` uppercase quirk and
        // `hookEvent` camelCase mapping.
        let att = &value["attachment"];
        assert_eq!(att["type"], "hook_success");
        assert_eq!(att["content"], "ok");
        assert_eq!(att["hookName"], "user-script");
        assert_eq!(att["toolUseID"], "toolu_01abc");
        assert_eq!(att["hookEvent"], "PreToolUse");
        assert_eq!(att["stdout"], "stdout output");
        assert!(
            att.as_object().unwrap().get("stderr").is_none(),
            "None optional fields must be omitted",
        );
        assert_eq!(att["exitCode"], 0);
        assert_eq!(att["command"], "bash ./hook.sh");
        assert_eq!(att["durationMs"], 42);

        // Round-trip.
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn hook_blocking_error_attachment_nests_blocking_error_struct() {
        let msg = Message::Attachment(AttachmentMessage {
            attachment: Attachment::HookBlockingError(HookBlockingErrorAttachment {
                blocking_error: HookBlockingError {
                    blocking_error: "denied by policy".to_string(),
                    command: "rm -rf /".to_string(),
                },
                hook_name: "guardrail".to_string(),
                tool_use_id: "toolu_02xyz".to_string(),
                hook_event: HookEvent::PreToolUse,
            }),
            uuid: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string(),
            timestamp: "2026-04-07T10:00:00.000Z".to_string(),
        });

        let value = serde_json::to_value(&msg).unwrap();
        let att = &value["attachment"];
        assert_eq!(att["type"], "hook_blocking_error");
        // Nested HookBlockingError serializes with blockingError (camelCase
        // via explicit rename, not rename_all).
        assert_eq!(
            att["blockingError"],
            json!({
                "blockingError": "denied by policy",
                "command": "rm -rf /"
            })
        );
        assert_eq!(att["toolUseID"], "toolu_02xyz");

        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn hook_permission_decision_attachment_uses_lowercase_decision() {
        let att = Attachment::HookPermissionDecision(HookPermissionDecisionAttachment {
            decision: HookPermissionDecision::Deny,
            tool_use_id: "toolu_03".to_string(),
            hook_event: HookEvent::PermissionRequest,
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["type"], "hook_permission_decision");
        assert_eq!(value["decision"], "deny");
        assert_eq!(value["toolUseID"], "toolu_03");
        assert_eq!(value["hookEvent"], "PermissionRequest");

        // Round-trip preserves variant.
        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn hook_result_message_alias_is_attachment_message() {
        // Compile-time proof that HookResultMessage is a pure alias.
        let hrm: HookResultMessage = AttachmentMessage {
            attachment: Attachment::HookAdditionalContext(HookAdditionalContextAttachment {
                content: vec!["line1".to_string(), "line2".to_string()],
                hook_name: "ctx".to_string(),
                tool_use_id: "toolu_04".to_string(),
                hook_event: HookEvent::UserPromptSubmit,
            }),
            uuid: "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee".to_string(),
            timestamp: "2026-04-07T11:00:00.000Z".to_string(),
        };
        assert!(hrm.attachment.is_hook_variant());
    }

    #[test]
    fn is_hook_variant_is_true_for_every_hook_attachment() {
        // Exhaustive check — every hook_* variant must return true from
        // `is_hook_variant()`. This test is paired with the non-hook
        // negative exhaustive test below; together they pin down the
        // full set of "hook" vs "non-hook" attachments.
        let events = HookEvent::PreToolUse;
        let variants: Vec<Attachment> = vec![
            Attachment::HookCancelled(HookCancelledAttachment {
                hook_name: "h".into(),
                tool_use_id: "t".into(),
                hook_event: events,
                command: None,
                duration_ms: None,
            }),
            Attachment::HookBlockingError(HookBlockingErrorAttachment {
                blocking_error: HookBlockingError {
                    blocking_error: "e".into(),
                    command: "c".into(),
                },
                hook_name: "h".into(),
                tool_use_id: "t".into(),
                hook_event: events,
            }),
            Attachment::HookNonBlockingError(HookNonBlockingErrorAttachment {
                hook_name: "h".into(),
                stderr: "".into(),
                stdout: "".into(),
                exit_code: 1,
                tool_use_id: "t".into(),
                hook_event: events,
                command: None,
                duration_ms: None,
            }),
            Attachment::HookErrorDuringExecution(HookErrorDuringExecutionAttachment {
                content: "".into(),
                hook_name: "h".into(),
                tool_use_id: "t".into(),
                hook_event: events,
                command: None,
                duration_ms: None,
            }),
            Attachment::HookStoppedContinuation(HookStoppedContinuationAttachment {
                message: "".into(),
                hook_name: "h".into(),
                tool_use_id: "t".into(),
                hook_event: events,
            }),
            Attachment::HookSuccess(HookSuccessAttachment {
                content: "".into(),
                hook_name: "h".into(),
                tool_use_id: "t".into(),
                hook_event: events,
                stdout: None,
                stderr: None,
                exit_code: None,
                command: None,
                duration_ms: None,
            }),
            Attachment::HookAdditionalContext(HookAdditionalContextAttachment {
                content: vec![],
                hook_name: "h".into(),
                tool_use_id: "t".into(),
                hook_event: events,
            }),
            Attachment::HookSystemMessage(HookSystemMessageAttachment {
                content: "".into(),
                hook_name: "h".into(),
                tool_use_id: "t".into(),
                hook_event: events,
            }),
            Attachment::HookPermissionDecision(HookPermissionDecisionAttachment {
                decision: HookPermissionDecision::Allow,
                tool_use_id: "t".into(),
                hook_event: events,
            }),
        ];
        for v in variants {
            assert!(
                v.is_hook_variant(),
                "variant {v:?} should be a hook variant"
            );
        }
    }

    // -----------------------------------------------------------------------
    // file-reference / plan-mode / auto-mode variants — round-trips + wire-key assertions.
    // -----------------------------------------------------------------------

    #[test]
    fn compact_file_reference_round_trips_with_display_path_camel_case() {
        let att = Attachment::CompactFileReference(CompactFileReferenceAttachment {
            filename: "notes.md".into(),
            display_path: "docs/notes.md".into(),
        });
        let value = serde_json::to_value(&att).unwrap();

        assert_eq!(value["type"], "compact_file_reference");
        assert_eq!(value["filename"], "notes.md");
        // Wire-key assertion: camelCase, not snake_case.
        assert_eq!(value["displayPath"], "docs/notes.md");
        assert!(
            value.as_object().unwrap().get("display_path").is_none(),
            "must not emit snake_case display_path"
        );

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn pdf_reference_round_trips_with_camel_case_keys() {
        let att = Attachment::PdfReference(PdfReferenceAttachment {
            filename: "report.pdf".into(),
            page_count: 42,
            file_size: 1024 * 1024,
            display_path: "artifacts/report.pdf".into(),
        });
        let value = serde_json::to_value(&att).unwrap();

        assert_eq!(value["type"], "pdf_reference");
        assert_eq!(value["filename"], "report.pdf");
        // camelCase wire keys.
        assert_eq!(value["pageCount"], 42);
        assert_eq!(value["fileSize"], 1024 * 1024);
        assert_eq!(value["displayPath"], "artifacts/report.pdf");

        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("page_count"));
        assert!(!obj.contains_key("file_size"));
        assert!(!obj.contains_key("display_path"));

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn pdf_reference_file_size_handles_values_above_i32_max() {
        // 3 GiB — comfortably above i32::MAX (≈ 2.147 GiB).
        let large: u64 = 3 * 1024 * 1024 * 1024;
        assert!(large > i32::MAX as u64);

        let att = Attachment::PdfReference(PdfReferenceAttachment {
            filename: "huge.pdf".into(),
            page_count: 12_345,
            file_size: large,
            display_path: "huge.pdf".into(),
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["fileSize"].as_u64(), Some(large));

        // Round-trip from raw JSON (bypassing Rust construction) to prove
        // the deserializer accepts u64 at the wire-level too.
        let raw = json!({
            "type": "pdf_reference",
            "filename": "huge.pdf",
            "pageCount": 12345,
            "fileSize": large,
            "displayPath": "huge.pdf"
        });
        let parsed: Attachment = serde_json::from_value(raw).unwrap();
        match parsed {
            Attachment::PdfReference(p) => {
                assert_eq!(p.file_size, large);
                assert_eq!(p.page_count, 12_345);
            }
            other => panic!("expected PdfReference, got {other:?}"),
        }
    }

    #[test]
    fn agent_mention_round_trips_with_agent_type_camel_case() {
        let att = Attachment::AgentMention(AgentMentionAttachment {
            agent_type: "general-purpose".into(),
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["type"], "agent_mention");
        assert_eq!(value["agentType"], "general-purpose");
        assert!(value.as_object().unwrap().get("agent_type").is_none());

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn edited_text_file_round_trips_and_preserves_embedded_newlines() {
        let snippet = "line one\nline two\r\nline three\n\tindented";
        let att = Attachment::EditedTextFile(EditedTextFileAttachment {
            filename: "src/main.rs".into(),
            snippet: snippet.into(),
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["type"], "edited_text_file");
        assert_eq!(value["filename"], "src/main.rs");
        assert_eq!(value["snippet"], snippet);

        // Serialize to string and confirm `\n` (escaped newline) appears
        // in the on-wire form — this exercises the JSON string encoder.
        let serialized = serde_json::to_string(&att).unwrap();
        assert!(
            serialized.contains("line one\\nline two"),
            "expected escaped newline in serialized form, got {serialized}",
        );

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
        if let Attachment::EditedTextFile(p) = back {
            assert_eq!(p.snippet, snippet);
        } else {
            unreachable!()
        }
    }

    #[test]
    fn plan_mode_full_reminder_round_trips_with_camel_case_keys() {
        let att = Attachment::PlanMode(PlanModeAttachment {
            reminder_type: ReminderType::Full,
            is_sub_agent: Some(true),
            plan_file_path: ".claude/plan.md".into(),
            plan_exists: true,
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["type"], "plan_mode");
        assert_eq!(value["reminderType"], "full");
        assert_eq!(value["isSubAgent"], true);
        assert_eq!(value["planFilePath"], ".claude/plan.md");
        assert_eq!(value["planExists"], true);

        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("reminder_type"));
        assert!(!obj.contains_key("is_sub_agent"));
        assert!(!obj.contains_key("plan_file_path"));
        assert!(!obj.contains_key("plan_exists"));

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn plan_mode_sparse_reminder_round_trips() {
        let att = Attachment::PlanMode(PlanModeAttachment {
            reminder_type: ReminderType::Sparse,
            is_sub_agent: None,
            plan_file_path: ".claude/plan.md".into(),
            plan_exists: false,
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["reminderType"], "sparse");
        assert_eq!(value["planExists"], false);

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn plan_mode_omits_is_sub_agent_when_none() {
        let att = Attachment::PlanMode(PlanModeAttachment {
            reminder_type: ReminderType::Full,
            is_sub_agent: None,
            plan_file_path: ".claude/plan.md".into(),
            plan_exists: true,
        });
        let value = serde_json::to_value(&att).unwrap();
        let obj = value.as_object().unwrap();
        assert!(
            !obj.contains_key("isSubAgent"),
            "isSubAgent must be omitted when None (mirrors JSON.stringify of undefined)",
        );

        // Round-trip preserves the None.
        let back: Attachment = serde_json::from_value(value).unwrap();
        if let Attachment::PlanMode(p) = back {
            assert!(p.is_sub_agent.is_none());
        } else {
            panic!("expected PlanMode");
        }
    }

    #[test]
    fn plan_mode_is_sub_agent_false_round_trips_as_explicit_false() {
        // `Some(false)` must survive the round trip as explicit `false`,
        // not be silently dropped.
        let att = Attachment::PlanMode(PlanModeAttachment {
            reminder_type: ReminderType::Full,
            is_sub_agent: Some(false),
            plan_file_path: ".claude/plan.md".into(),
            plan_exists: true,
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["isSubAgent"], false);

        // And from raw wire form with explicit false.
        let raw = json!({
            "type": "plan_mode",
            "reminderType": "full",
            "isSubAgent": false,
            "planFilePath": ".claude/plan.md",
            "planExists": true
        });
        let parsed: Attachment = serde_json::from_value(raw).unwrap();
        if let Attachment::PlanMode(p) = parsed {
            assert_eq!(p.is_sub_agent, Some(false));
        } else {
            panic!("expected PlanMode");
        }
    }

    #[test]
    fn plan_mode_unknown_reminder_type_falls_to_other() {
        // The literal "verbose" is not one of `"full"` / `"sparse"` —
        // strict serde would parse-error; our open-world Attachment
        // fallback must route the whole row into `Attachment::Other`
        // instead.
        let raw = json!({
            "type": "plan_mode",
            "reminderType": "verbose",
            "isSubAgent": true,
            "planFilePath": ".claude/plan.md",
            "planExists": true
        });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match parsed {
            Attachment::Other(v) => assert_eq!(v, raw),
            other => panic!("expected Attachment::Other, got {other:?}"),
        }
    }

    #[test]
    fn plan_mode_reentry_round_trips() {
        let att = Attachment::PlanModeReentry(PlanModeReentryAttachment {
            plan_file_path: ".claude/plan.md".into(),
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["type"], "plan_mode_reentry");
        assert_eq!(value["planFilePath"], ".claude/plan.md");

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn plan_mode_exit_round_trips() {
        let att = Attachment::PlanModeExit(PlanModeExitAttachment {
            plan_file_path: ".claude/plan.md".into(),
            plan_exists: false,
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["type"], "plan_mode_exit");
        assert_eq!(value["planFilePath"], ".claude/plan.md");
        assert_eq!(value["planExists"], false);

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn auto_mode_round_trips_with_reminder_type_camel_case() {
        let att = Attachment::AutoMode(AutoModeAttachment {
            reminder_type: ReminderType::Sparse,
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["type"], "auto_mode");
        assert_eq!(value["reminderType"], "sparse");

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn auto_mode_unknown_reminder_type_falls_to_other() {
        let raw = json!({
            "type": "auto_mode",
            "reminderType": "verbose"
        });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match parsed {
            Attachment::Other(v) => assert_eq!(v, raw),
            other => panic!("expected Attachment::Other, got {other:?}"),
        }
    }

    #[test]
    fn auto_mode_exit_serializes_as_type_only_object() {
        let att = Attachment::AutoModeExit;
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value, json!({ "type": "auto_mode_exit" }));

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    // -----------------------------------------------------------------------
    // directory + IDE file-reference variants.
    //
    // Each new typed variant gets:
    //   1. A round-trip test from a constructed Rust value, asserting the
    //      camelCase wire keys (`displayPath`, `lineStart`, `lineEnd`,
    //      `ideName`) match the wire format.
    //   2. An inverse round-trip from a raw `json!` value, proving the
    //      deserializer accepts wire input as emitted, without any
    //      Rust-side construction.
    //   3. A "must take typed path, not Other" sanity guard inside the
    //      `known_discriminator_*` test below, paired with the existing
    //      hook / file-reference sanity guard.
    //
    // Plus the negative coverage of `is_hook_variant`, the existing mixed
    // transcript fixture (extended with directory + selected_lines_in_ide
    // + opened_file_in_ide rows), and a regression test for the documented
    // u64 line-number rule on `selected_lines_in_ide`.
    // -----------------------------------------------------------------------

    #[test]
    fn directory_attachment_round_trips_with_camel_case_display_path() {
        let att = Attachment::Directory(DirectoryAttachment {
            path: "/abs/work/project/src".into(),
            content: "src/\n  main.rs\n  lib.rs".into(),
            display_path: "src".into(),
        });
        let value = serde_json::to_value(&att).unwrap();

        assert_eq!(value["type"], "directory");
        assert_eq!(value["path"], "/abs/work/project/src");
        assert_eq!(value["content"], "src/\n  main.rs\n  lib.rs");
        // Wire-key assertion: camelCase, not snake_case.
        assert_eq!(value["displayPath"], "src");

        let obj = value.as_object().unwrap();
        assert!(
            !obj.contains_key("display_path"),
            "must not emit snake_case display_path",
        );

        // Round-trip preserves the variant.
        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn directory_attachment_accepts_wire_form_from_raw_json() {
        // Inverse round-trip: deserialize a raw `json!` value (the shape a
        // `directory` attachment row has in a transcript) and assert
        // the typed variant lands with the right field values.
        let raw = json!({
            "type": "directory",
            "path": "/repo/docs",
            "content": "docs/\n  guide.md\n  api.md\n",
            "displayPath": "docs"
        });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match parsed {
            Attachment::Directory(p) => {
                assert_eq!(p.path, "/repo/docs");
                assert_eq!(p.content, "docs/\n  guide.md\n  api.md\n");
                assert_eq!(p.display_path, "docs");
            }
            other => panic!("expected Attachment::Directory, got {other:?}"),
        }

        // And the round trip back to JSON is byte-for-byte equal to the input.
        let reparsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&reparsed).unwrap(), raw);
    }

    #[test]
    fn selected_lines_in_ide_round_trips_with_all_camel_case_keys() {
        let att = Attachment::SelectedLinesInIde(SelectedLinesInIdeAttachment {
            ide_name: "VSCode".into(),
            line_start: 12,
            line_end: 27,
            filename: "src/main.rs".into(),
            content: "fn main() {\n    println!(\"hi\");\n}".into(),
            display_path: "src/main.rs".into(),
        });
        let value = serde_json::to_value(&att).unwrap();

        assert_eq!(value["type"], "selected_lines_in_ide");
        // Every camelCase key of `SelectedLinesInIdeAttachment`.
        assert_eq!(value["ideName"], "VSCode");
        assert_eq!(value["lineStart"], 12);
        assert_eq!(value["lineEnd"], 27);
        assert_eq!(value["filename"], "src/main.rs");
        assert_eq!(value["content"], "fn main() {\n    println!(\"hi\");\n}");
        assert_eq!(value["displayPath"], "src/main.rs");

        // No snake_case fall-through.
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("ide_name"));
        assert!(!obj.contains_key("line_start"));
        assert!(!obj.contains_key("line_end"));
        assert!(!obj.contains_key("display_path"));

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn selected_lines_in_ide_accepts_wire_form_from_raw_json() {
        let raw = json!({
            "type": "selected_lines_in_ide",
            "ideName": "Cursor",
            "lineStart": 1,
            "lineEnd": 1,
            "filename": "README.md",
            "content": "# Hello",
            "displayPath": "README.md"
        });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match parsed {
            Attachment::SelectedLinesInIde(p) => {
                assert_eq!(p.ide_name, "Cursor");
                assert_eq!(p.line_start, 1);
                assert_eq!(p.line_end, 1);
                assert_eq!(p.filename, "README.md");
                assert_eq!(p.content, "# Hello");
                assert_eq!(p.display_path, "README.md");
            }
            other => panic!("expected SelectedLinesInIde, got {other:?}"),
        }

        let reparsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&reparsed).unwrap(), raw);
    }

    #[test]
    fn selected_lines_in_ide_line_numbers_handle_values_above_i32_max() {
        // Same load-bearing rule as
        // `pdf_reference_file_size_handles_values_above_i32_max`: the wire's
        // plain `number` is f64-backed so the wire shape carries no upper bound,
        // and Rust must accept line numbers above i32::MAX rather than
        // silently truncating. Generated transcripts in long-running
        // sessions or non-text streams can plausibly trip this.
        let large: u64 = (i32::MAX as u64) + 1_000;
        assert!(large > i32::MAX as u64);

        let att = Attachment::SelectedLinesInIde(SelectedLinesInIdeAttachment {
            ide_name: "Generated".into(),
            line_start: large,
            line_end: large + 5,
            filename: "huge.log".into(),
            content: "...".into(),
            display_path: "huge.log".into(),
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(value["lineStart"].as_u64(), Some(large));
        assert_eq!(value["lineEnd"].as_u64(), Some(large + 5));

        // Inverse: bypass Rust construction and prove the wire-level
        // deserializer accepts u64 too.
        let raw = json!({
            "type": "selected_lines_in_ide",
            "ideName": "Generated",
            "lineStart": large,
            "lineEnd": large + 5,
            "filename": "huge.log",
            "content": "...",
            "displayPath": "huge.log"
        });
        let parsed: Attachment = serde_json::from_value(raw).unwrap();
        match parsed {
            Attachment::SelectedLinesInIde(p) => {
                assert_eq!(p.line_start, large);
                assert_eq!(p.line_end, large + 5);
            }
            other => panic!("expected SelectedLinesInIde, got {other:?}"),
        }
    }

    #[test]
    fn opened_file_in_ide_round_trips_with_only_filename_field() {
        let att = Attachment::OpenedFileInIde(OpenedFileInIdeAttachment {
            filename: "src/lib.rs".into(),
        });
        let value = serde_json::to_value(&att).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "opened_file_in_ide",
                "filename": "src/lib.rs"
            })
        );

        // Object has exactly two keys: `type` and `filename`. The row carries
        // nothing else, so any
        // extra Rust-side key would be a wire-shape divergence.
        let obj = value.as_object().unwrap();
        assert_eq!(
            obj.len(),
            2,
            "opened_file_in_ide must serialize as type + filename only"
        );
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("filename"));

        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn batch_3a_known_discriminators_take_typed_path_not_other() {
        // Sanity guard paired with `known_discriminator_deserializes_to_typed_variant_not_other`
        // below for the hook and file-reference variants. A future refactor that accidentally pushes
        // any of these three variants through the `Other` fallback would
        // be caught here.
        let cases: Vec<(JsonValue, &'static str)> = vec![
            (
                json!({
                    "type": "directory",
                    "path": "/p",
                    "content": "c",
                    "displayPath": "p"
                }),
                "directory",
            ),
            (
                json!({
                    "type": "selected_lines_in_ide",
                    "ideName": "i",
                    "lineStart": 1,
                    "lineEnd": 2,
                    "filename": "f",
                    "content": "c",
                    "displayPath": "f"
                }),
                "selected_lines_in_ide",
            ),
            (
                json!({
                    "type": "opened_file_in_ide",
                    "filename": "f"
                }),
                "opened_file_in_ide",
            ),
        ];

        for (raw, label) in cases {
            let parsed: Attachment = serde_json::from_value(raw).unwrap();
            match (label, &parsed) {
                ("directory", Attachment::Directory(_)) => {}
                ("selected_lines_in_ide", Attachment::SelectedLinesInIde(_)) => {}
                ("opened_file_in_ide", Attachment::OpenedFileInIde(_)) => {}
                _ => panic!("known {label} silently fell into {parsed:?}"),
            }
        }
    }

    #[test]
    fn malformed_directory_attachment_falls_to_other() {
        // Edge case: a `directory` row missing required fields. The
        // open-world fallback must route the row into `Attachment::Other`
        // (preserving the raw JSON for downstream debugging) instead of
        // returning a parse error. Matches `malformed_known_attachment_falls_to_other`
        // for the new variant.
        let raw = json!({ "type": "directory", "path": "/p" });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match &parsed {
            Attachment::Other(v) => assert_eq!(v, &raw),
            other => panic!("malformed directory must fall to Attachment::Other, got {other:?}"),
        }
    }

    #[test]
    fn malformed_selected_lines_in_ide_attachment_falls_to_other() {
        // `selected_lines_in_ide` requires six fields. Drop most of them
        // and prove the row still survives via the open-world fallback.
        let raw = json!({
            "type": "selected_lines_in_ide",
            "ideName": "VSCode"
        });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match &parsed {
            Attachment::Other(v) => assert_eq!(v, &raw),
            other => panic!(
                "malformed selected_lines_in_ide must fall to Attachment::Other, got {other:?}"
            ),
        }
    }

    // -----------------------------------------------------------------------
    // Open-world fallback behaviour.
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_attachment_discriminator_preserved_as_other() {
        let raw = json!({
            "type": "totally_future_attachment_type",
            "foo": "bar",
            "nested": { "x": 1, "y": [1, 2, 3] }
        });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match &parsed {
            Attachment::Other(v) => assert_eq!(v, &raw),
            other => panic!("expected Attachment::Other, got {other:?}"),
        }

        // Lossless round-trip: re-serializing must yield the same raw JSON.
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(reserialized, raw);
    }

    #[test]
    fn malformed_known_attachment_falls_to_other() {
        // `hook_success` requires `content`, `hookName`, `toolUseID`,
        // `hookEvent`. Omit all of them except the discriminator. A strict
        // typed dispatch would parse-error — the open-world fallback must
        // route the row into `Attachment::Other` and preserve it unchanged.
        let raw = json!({ "type": "hook_success" });
        let parsed: Attachment = serde_json::from_value(raw.clone()).unwrap();
        match &parsed {
            Attachment::Other(v) => assert_eq!(v, &raw),
            other => panic!("malformed hook_success must fall to Attachment::Other, got {other:?}"),
        }
    }

    #[test]
    fn known_discriminator_deserializes_to_typed_variant_not_other() {
        // Sanity check: valid known rows must take the typed path, never
        // silently slip into Other.
        let raw = json!({
            "type": "agent_mention",
            "agentType": "Explore"
        });
        let parsed: Attachment = serde_json::from_value(raw).unwrap();
        match parsed {
            Attachment::AgentMention(p) => assert_eq!(p.agent_type, "Explore"),
            other => panic!("expected AgentMention, got {other:?}"),
        }

        // Also check a hook variant to guard against a future refactor
        // that accidentally pushes hooks into Other.
        let raw = json!({
            "type": "hook_system_message",
            "content": "msg",
            "hookName": "h",
            "toolUseID": "t",
            "hookEvent": "Notification"
        });
        let parsed: Attachment = serde_json::from_value(raw).unwrap();
        match parsed {
            Attachment::HookSystemMessage(_) => {}
            other => panic!("expected HookSystemMessage, got {other:?}"),
        }
    }

    #[test]
    fn legacy_attachment_types_parse_as_attachment_other() {
        // The five removed attachment types.
        // Every one must round-trip through Attachment::Other without error.
        let legacy_types = [
            "autocheckpointing",
            "background_task_status",
            "todo",
            "task_progress",
            "ultramemory",
        ];
        for ty in legacy_types {
            let raw = json!({
                "type": ty,
                "legacy_field": "whatever",
                "extra": [1, 2, 3]
            });
            let parsed: Attachment = serde_json::from_value(raw.clone())
                .unwrap_or_else(|e| panic!("legacy type {ty} failed to parse: {e}"));
            match parsed {
                Attachment::Other(v) => assert_eq!(v, raw, "legacy {ty} must preserve raw JSON"),
                other => panic!("legacy {ty} must land in Other, got {other:?}"),
            }
        }
    }

    #[test]
    fn legacy_top_level_progress_parses_as_message_other() {
        // `progress` is no longer a live transcript row type, but old
        // transcripts still contain `"type": "progress"` rows that the
        // Rust side must absorb via Message::Other.
        let raw = json!({
            "type": "progress",
            "uuid": "11111111-1111-1111-1111-111111111111",
            "parentUuid": "22222222-2222-2222-2222-222222222222",
            "content": "legacy progress row"
        });
        let parsed: Message = serde_json::from_value(raw.clone()).unwrap();
        match &parsed {
            Message::Other(v) => assert_eq!(v, &raw),
            other => panic!("expected Message::Other, got {other:?}"),
        }
        // Lossless round-trip.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
    }

    #[test]
    fn unknown_system_subtype_parses_as_system_message_other() {
        let raw = json!({
            "type": "system",
            "subtype": "thinking",
            "content": "internal musing",
            "timestamp": "2026-04-07T12:00:00.000Z",
            "uuid": "33333333-3333-3333-3333-333333333333"
        });
        let parsed: Message = serde_json::from_value(raw.clone()).unwrap();
        match &parsed {
            Message::System(SystemMessage::Other(v)) => assert_eq!(v, &raw),
            other => panic!("expected Message::System(SystemMessage::Other), got {other:?}"),
        }
        // Lossless round-trip.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
    }

    #[test]
    fn mixed_transcript_fixture_parses_all_lines_with_expected_variant_mix() {
        // Several JSONL-ish rows that together exercise system + hook rows,
        // the file-reference / plan-mode / auto-mode variants, legacy
        // attachments, a legacy progress row, and an unknown future
        // attachment. Every line must parse and the counts of typed vs
        // Other rows must match expectation.
        let rows: Vec<JsonValue> = vec![
            // system api_error
            api_error_fixture(),
            // attachment -> hook_success
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "hook_success",
                    "content": "ok",
                    "hookName": "h",
                    "toolUseID": "t",
                    "hookEvent": "PreToolUse"
                },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000001",
                "timestamp": "2026-04-07T01:00:00.000Z"
            }),
            // attachment -> plan_mode with full reminder
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "plan_mode",
                    "reminderType": "full",
                    "isSubAgent": false,
                    "planFilePath": ".claude/plan.md",
                    "planExists": true
                },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000002",
                "timestamp": "2026-04-07T02:00:00.000Z"
            }),
            // attachment -> auto_mode_exit (unit)
            json!({
                "type": "attachment",
                "attachment": { "type": "auto_mode_exit" },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000003",
                "timestamp": "2026-04-07T03:00:00.000Z"
            }),
            // attachment -> pdf_reference
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "pdf_reference",
                    "filename": "report.pdf",
                    "pageCount": 10,
                    "fileSize": 500000,
                    "displayPath": "artifacts/report.pdf"
                },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000004",
                "timestamp": "2026-04-07T04:00:00.000Z"
            }),
            // Legacy attachment: ultramemory
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "ultramemory",
                    "legacy": true
                },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000005",
                "timestamp": "2026-04-07T05:00:00.000Z"
            }),
            // Legacy top-level row: progress
            json!({
                "type": "progress",
                "uuid": "aaaaaaaa-0000-0000-0000-000000000006",
                "parentUuid": "aaaaaaaa-0000-0000-0000-000000000005"
            }),
            // Unknown future attachment
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "quantum_reference",
                    "probability": 0.42
                },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000007",
                "timestamp": "2026-04-07T07:00:00.000Z"
            }),
            // directory
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "directory",
                    "path": "/abs/work/project/src",
                    "content": "src/\n  main.rs\n  lib.rs",
                    "displayPath": "src"
                },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000008",
                "timestamp": "2026-04-07T08:00:00.000Z"
            }),
            // selected_lines_in_ide
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "selected_lines_in_ide",
                    "ideName": "VSCode",
                    "lineStart": 12,
                    "lineEnd": 27,
                    "filename": "src/main.rs",
                    "content": "fn main() {}",
                    "displayPath": "src/main.rs"
                },
                "uuid": "aaaaaaaa-0000-0000-0000-000000000009",
                "timestamp": "2026-04-07T09:00:00.000Z"
            }),
            // opened_file_in_ide
            json!({
                "type": "attachment",
                "attachment": {
                    "type": "opened_file_in_ide",
                    "filename": "src/lib.rs"
                },
                "uuid": "aaaaaaaa-0000-0000-0000-00000000000a",
                "timestamp": "2026-04-07T10:00:00.000Z"
            }),
        ];

        let parsed: Vec<Message> = rows
            .iter()
            .map(|row| {
                serde_json::from_value(row.clone())
                    .unwrap_or_else(|e| panic!("failed to parse {row}: {e}"))
            })
            .collect();

        // Count classifications.
        let mut typed_system = 0usize;
        let mut typed_attachment_hook = 0usize;
        let mut typed_attachment_batch2 = 0usize;
        let mut typed_attachment_batch3a = 0usize;
        let mut attachment_other_nested = 0usize;
        let mut message_other = 0usize;
        for msg in &parsed {
            match msg {
                Message::System(SystemMessage::ApiError(_)) => typed_system += 1,
                Message::Attachment(AttachmentMessage { attachment, .. }) => match attachment {
                    Attachment::HookSuccess(_)
                    | Attachment::HookBlockingError(_)
                    | Attachment::HookCancelled(_)
                    | Attachment::HookAdditionalContext(_)
                    | Attachment::HookErrorDuringExecution(_)
                    | Attachment::HookNonBlockingError(_)
                    | Attachment::HookPermissionDecision(_)
                    | Attachment::HookStoppedContinuation(_)
                    | Attachment::HookSystemMessage(_) => typed_attachment_hook += 1,
                    Attachment::PlanMode(_)
                    | Attachment::PdfReference(_)
                    | Attachment::AutoModeExit => typed_attachment_batch2 += 1,
                    Attachment::Directory(_)
                    | Attachment::SelectedLinesInIde(_)
                    | Attachment::OpenedFileInIde(_) => typed_attachment_batch3a += 1,
                    Attachment::Other(_) => attachment_other_nested += 1,
                    other => panic!("unexpected attachment variant: {other:?}"),
                },
                Message::Other(_) => message_other += 1,
                other => panic!("unexpected message variant: {other:?}"),
            }
        }

        assert_eq!(typed_system, 1, "api_error");
        assert_eq!(typed_attachment_hook, 1, "hook_success");
        assert_eq!(
            typed_attachment_batch2, 3,
            "plan_mode + auto_mode_exit + pdf_reference"
        );
        assert_eq!(
            typed_attachment_batch3a, 3,
            "directory + selected_lines_in_ide + opened_file_in_ide",
        );
        // ultramemory (legacy) and quantum_reference (future) both land in
        // Attachment::Other but remain wrapped in a Message::Attachment.
        assert_eq!(
            attachment_other_nested, 2,
            "ultramemory + quantum_reference"
        );
        // Legacy top-level `progress` is a Message::Other.
        assert_eq!(message_other, 1, "legacy progress row");

        // Every row round-trips losslessly.
        for (row, msg) in rows.iter().zip(parsed.iter()) {
            let back = serde_json::to_value(msg).unwrap();
            assert_eq!(&back, row, "mixed fixture row did not round-trip");
        }
    }

    // -----------------------------------------------------------------------
    // is_hook_variant exhaustive negative coverage for the non-hook variants.
    // -----------------------------------------------------------------------

    #[test]
    fn is_hook_variant_is_false_for_every_non_hook_attachment() {
        // Every non-hook variant (file-reference / plan-mode / auto-mode +
        // directory / IDE + the Other fallback) must report false. Paired
        // with the hook positive test above so
        // `is_hook_variant` is pinned from both sides for the full
        // attachment universe covered here.
        let variants: Vec<Attachment> = vec![
            // file-reference / plan-mode / auto-mode.
            Attachment::CompactFileReference(CompactFileReferenceAttachment {
                filename: "f".into(),
                display_path: "f".into(),
            }),
            Attachment::PdfReference(PdfReferenceAttachment {
                filename: "f".into(),
                page_count: 1,
                file_size: 1,
                display_path: "f".into(),
            }),
            Attachment::AgentMention(AgentMentionAttachment {
                agent_type: "a".into(),
            }),
            Attachment::EditedTextFile(EditedTextFileAttachment {
                filename: "f".into(),
                snippet: "s".into(),
            }),
            Attachment::PlanMode(PlanModeAttachment {
                reminder_type: ReminderType::Full,
                is_sub_agent: None,
                plan_file_path: "p".into(),
                plan_exists: true,
            }),
            Attachment::PlanModeReentry(PlanModeReentryAttachment {
                plan_file_path: "p".into(),
            }),
            Attachment::PlanModeExit(PlanModeExitAttachment {
                plan_file_path: "p".into(),
                plan_exists: false,
            }),
            Attachment::AutoMode(AutoModeAttachment {
                reminder_type: ReminderType::Sparse,
            }),
            Attachment::AutoModeExit,
            // directory + IDE file references.
            Attachment::Directory(DirectoryAttachment {
                path: "/p".into(),
                content: "c".into(),
                display_path: "p".into(),
            }),
            Attachment::SelectedLinesInIde(SelectedLinesInIdeAttachment {
                ide_name: "i".into(),
                line_start: 1,
                line_end: 2,
                filename: "f".into(),
                content: "c".into(),
                display_path: "f".into(),
            }),
            Attachment::OpenedFileInIde(OpenedFileInIdeAttachment {
                filename: "f".into(),
            }),
            // Open-world fallback.
            Attachment::Other(json!({"type": "unknown_future"})),
        ];
        for v in variants {
            assert!(
                !v.is_hook_variant(),
                "variant {v:?} must NOT be reported as a hook variant",
            );
        }
    }
}
