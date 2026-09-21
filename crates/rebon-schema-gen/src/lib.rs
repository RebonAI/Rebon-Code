//! The ACP wire types, exported as JSON Schema and as the TypeScript the
//! browser client reads.
//!
//! `rebon-web-ui` used to carry a hand-written mirror of these types. A mirror
//! drifts silently: nothing fails when a Rust field is renamed and the
//! TypeScript is not, so the page keeps compiling against a shape the server
//! stopped sending. Here the TypeScript is *generated* from the same
//! definitions the server serializes, and a test compares the checked-in file
//! against a fresh run, so drift is a red test instead of a runtime surprise.
//!
//! Two things are deliberately **not** here:
//!
//! - `/api/commands`, whose `commands` array carries two shapes — a catalog
//!   command and a skill entry — so the Rust side holds `Value` and the page
//!   narrows it to the one it renders. Generating it would replace a useful
//!   narrowing with `JsonValue[]`.
//! - The client's own leniency. The page accepts a `session/update` it does
//!   not know and both spellings of the fields an older server sent; that is
//!   the client's policy, not the wire's, so it lives in the `@rebon/rebon-web`
//!   package on top of what is generated here.
//!
//! Not everything here is read by the browser. The `_session/*` extension
//! (the internal control plane) is generated for the other half of the
//! argument: a wire that exists only as Rust types cannot be checked by
//! anyone who is not compiling this workspace, and the schema is where it
//! gets written down. The page ignores those definitions.
//!
//! Regenerate with `cargo run -p rebon-schema-gen`.

mod typescript;

use std::collections::BTreeMap;

use rebon_session_host::session_ext;
use schemars::{schema_for, Schema};
use serde_json::Value;

/// Where the generated files live, relative to the repository root.
pub const SCHEMA_PATH: &str = "assets/schemas/acp-wire.json";
pub const TYPESCRIPT_PATH: &str = "assets/web-ui/acp/types.generated.ts";

/// The command that rewrites both files, quoted in their headers and in the
/// failure message of the staleness test.
pub const REGENERATE_COMMAND: &str = "cargo run -p rebon-schema-gen";

/// Every type the browser client reads off the wire.
///
/// Adding a root here is how a new wire type reaches the page: the closure of
/// its fields comes along on its own, because `schema_for!` walks them.
fn roots() -> Vec<(&'static str, Schema)> {
    vec![
        ("JsonRpcError", schema_for!(rebon_proto::JsonRpcError)),
        // `rebon serve`'s own `/api/*` reads. Not ACP, but the page reads
        // them off the same server and used to carry a hand-written mirror
        // of every one.
        ("ServerInfo", schema_for!(rebon_proto::web_api::ServerInfo)),
        (
            "SkillsResponse",
            schema_for!(rebon_proto::web_api::SkillsResponse),
        ),
        (
            "AgentsResponse",
            schema_for!(rebon_proto::web_api::AgentsResponse),
        ),
        (
            "TasksResponse",
            schema_for!(rebon_proto::web_api::TasksResponse),
        ),
        (
            "ModelsResponse",
            schema_for!(rebon_proto::web_api::ModelsResponse),
        ),
        (
            "ActionResult",
            schema_for!(rebon_proto::web_api::ActionResult),
        ),
        (
            "FilesResponse",
            schema_for!(rebon_proto::web_api::FilesResponse),
        ),
        (
            "UsageResponse",
            schema_for!(rebon_proto::web_api::UsageResponse),
        ),
        (
            "RewindResponse",
            schema_for!(rebon_proto::web_api::RewindResponse),
        ),
        (
            "HistorySnapshot",
            schema_for!(rebon_proto::web_api::HistorySnapshot),
        ),
        (
            "PluginStatus",
            schema_for!(rebon_proto::web_api::PluginStatus),
        ),
        ("McpStatus", schema_for!(rebon_proto::web_api::McpStatus)),
        ("ContentBlock", schema_for!(rebon_types::ContentBlock)),
        (
            "InitializeResult",
            schema_for!(rebon_proto::InitializeResult),
        ),
        ("SessionInfo", schema_for!(rebon_proto::SessionInfo)),
        (
            "SessionNewResult",
            schema_for!(rebon_proto::SessionNewResult),
        ),
        (
            "SessionLoadResult",
            schema_for!(rebon_proto::SessionLoadResult),
        ),
        (
            "SessionListResult",
            schema_for!(rebon_proto::SessionListResult),
        ),
        (
            "SessionPromptResult",
            schema_for!(rebon_proto::SessionPromptResult),
        ),
        (
            "SessionSetConfigOptionResult",
            schema_for!(rebon_proto::SessionSetConfigOptionResult),
        ),
        (
            "SessionSteeringResult",
            schema_for!(rebon_proto::types::SessionSteeringResult),
        ),
        (
            "SessionUpdateParams",
            schema_for!(rebon_types::SessionUpdateParams),
        ),
        (
            "RequestPermissionParams",
            schema_for!(rebon_proto::RequestPermissionParams),
        ),
        (
            "RequestPermissionWireResult",
            schema_for!(rebon_proto::types::RequestPermissionWireResult),
        ),
        ("SlashCommand", schema_for!(rebon_types::SlashCommand)),
        ("ConfigOption", schema_for!(rebon_types::ConfigOption)),
        // The one display row. Not an ACP type — `/api/history` serves it —
        // but the page reads it off the wire just the same, and the producer
        // lives in a crate every surface can reach, precisely so the page
        // stops folding the transcript its own way.
        (
            "TranscriptRow",
            schema_for!(rebon_render::transcript_row::Message),
        ),
        // The `_session/*` extension: rebon's internal control plane, which
        // is expressed as ACP plus these extensions. The browser
        // does not read them; they are here because they are a wire, and the
        // schema is where a wire is written down for a reader who is not
        // compiling this crate.
        //
        // Two labels differ from their Rust names. Inside `session_ext` a
        // method that takes nothing is `NoParams` and one that answers only
        // "done" is `Ack`, which read fine next to the method they belong to;
        // in one flat table of ninety-odd definitions they read as nothing at
        // all. Relabelling a root is what the first element of these pairs is
        // for — `TranscriptRow` above is the same move.
        ("RebonMeta", schema_for!(session_ext::RebonMeta)),
        ("SessionExtNoParams", schema_for!(session_ext::NoParams)),
        ("SessionExtAck", schema_for!(session_ext::Ack)),
        (
            "RunCommandParams",
            schema_for!(session_ext::RunCommandParams),
        ),
        (
            "RunCommandResult",
            schema_for!(session_ext::RunCommandResult),
        ),
        ("StatusResult", schema_for!(session_ext::StatusResult)),
        (
            "SetPermissionModeParams",
            schema_for!(session_ext::SetPermissionModeParams),
        ),
        ("SetOptionParams", schema_for!(session_ext::SetOptionParams)),
        ("RewindParams", schema_for!(session_ext::RewindParams)),
        ("CompactParams", schema_for!(session_ext::CompactParams)),
        (
            "AnswerQuestionsParams",
            schema_for!(session_ext::AnswerQuestionsParams),
        ),
        ("TaskReplyParams", schema_for!(session_ext::TaskReplyParams)),
        (
            "CancelTasksParams",
            schema_for!(session_ext::CancelTasksParams),
        ),
        ("LeaseParams", schema_for!(session_ext::LeaseParams)),
        (
            "ReleaseLeaseParams",
            schema_for!(session_ext::ReleaseLeaseParams),
        ),
        ("SubscribeParams", schema_for!(session_ext::SubscribeParams)),
        (
            "CancelCallParams",
            schema_for!(session_ext::CancelCallParams),
        ),
        (
            "CancelCallResult",
            schema_for!(session_ext::CancelCallResult),
        ),
        ("HelloParams", schema_for!(session_ext::HelloParams)),
        ("TurnParams", schema_for!(session_ext::TurnParams)),
        (
            "StatusChangedParams",
            schema_for!(session_ext::StatusChangedParams),
        ),
        ("GapParams", schema_for!(session_ext::GapParams)),
    ]
}

/// Every named definition, root types included, keyed by type name.
///
/// `schema_for!` puts a root's own body inline and its field types under
/// `$defs`; folding the roots back in gives one flat table to emit from, and
/// the `BTreeMap` fixes the order so two runs produce the same bytes.
pub fn definitions() -> BTreeMap<String, Value> {
    let mut defs: BTreeMap<String, Value> = BTreeMap::new();
    for (name, schema) in roots() {
        let mut body = schema.to_value();
        let object = body
            .as_object_mut()
            .expect("schema_for! always produces an object");
        object.remove("$schema");
        object.remove("title");
        if let Some(Value::Object(nested)) = object.remove("$defs") {
            for (key, value) in nested {
                defs.insert(key, value);
            }
        }
        defs.insert(name.to_string(), body);
    }
    defs
}

/// The files to write, in the order `main` writes them.
pub fn artefacts() -> Vec<(&'static str, String)> {
    let defs = definitions();
    vec![
        (SCHEMA_PATH, json_schema_document(&defs)),
        (TYPESCRIPT_PATH, typescript::render(&defs)),
    ]
}

/// The definitions as one JSON Schema document.
///
/// Kept beside the TypeScript so a second client — the Flutter one, when its
/// protocol is folded into these types — has a language-neutral input rather
/// than a reason to grow a second exporter.
fn json_schema_document(defs: &BTreeMap<String, Value>) -> String {
    let document = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "rebon ACP wire types",
        "description": format!("Generated by `{REGENERATE_COMMAND}` from crates/rebon-proto, crates/rebon-types and crates/rebon-session-host. Do not edit by hand."),
        "$defs": defs,
    });
    let mut text = serde_json::to_string_pretty(&document).expect("schema serializes");
    text.push('\n');
    text
}
