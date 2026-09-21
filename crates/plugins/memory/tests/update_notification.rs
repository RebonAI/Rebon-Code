//! The `Memory updated in … · /memory to edit` line, end to end on the seat.
//!
//! The line is produced by one subscriber rather than inside `Write`, `Edit`
//! and `MultiEdit`. What the tests pin is unchanged:
//! writing into an auto-memory directory yields the notification under
//! `memoryNotification`, formatted against the platform home the way every
//! other `~` in the process is, and writing anywhere else yields nothing.
//!
//! Two things are new and deliberate. The subscriber leaves with the plugin,
//! so switching `memory` off removes the line — there is no `/memory` to edit
//! without the feature. And the class of tools it applies to comes from
//! `rebon-tools-core`'s builtin table rather than three hand-written copies,
//! so `NotebookEdit` — same permission class, same auto-memory carve-out — is
//! now annotated too.

mod support;

use std::sync::Arc;

use rebon_core::turn_hook::{ToolAnnotationHookContext, ToolAnnotationHookEvent, TurnHook};
use rebon_plugin_memory::{MemoryUpdateNotification, MEMORY_NOTIFICATION_FIELD};
use serde_json::{json, Value};
use support::{boot, Booted, HomeGuard};

/// Ask the plugin's subscriber for its fields and merge them into a tool
/// result the way the runtime does.
///
/// The subscriber has to be on the seat for this to mean anything, so that is
/// asserted first. The merge itself is `TurnHookRuntime`'s and is covered in
/// `rebon-core`; it is not reachable from here.
fn annotate(booted: &Booted, tool: &str, input: Value, cwd: &str) -> Value {
    assert!(
        booted
            .turn_hook_seat()
            .subscriber_ids()
            .iter()
            .any(|id| id == rebon_plugin_memory::update_hook::HOOK_ID),
        "the plugin's subscriber is on the seat"
    );
    let hook: Arc<dyn TurnHook> = Arc::new(MemoryUpdateNotification);
    let mut context = ToolAnnotationHookContext::default();
    hook.annotate_tool_result(
        &ToolAnnotationHookEvent {
            tool_name: tool,
            input: &input,
            cwd: Some(cwd),
        },
        &mut context,
    );
    let mut output = json!({"type": "update"});
    let object = output.as_object_mut().expect("an object");
    for (key, value) in context.fields() {
        object.insert(key.clone(), value.clone());
    }
    output
}

/// The line the pre-move tools produced, formatted the same way.
fn expected_line(memory_file: &str, cwd: &str) -> String {
    let home = rebon_session::platform_home_dir()
        .map(|home| home.to_string_lossy().into_owned())
        .unwrap_or_default();
    rebon_plugin_memory::memory::update_notification::format_update_notification(
        memory_file,
        &home,
        cwd,
    )
}

/// Every file-edit tool that writes into an auto-memory directory gets the
/// line, keyed by the path field its own table entry declares.
#[test]
fn every_file_edit_tool_annotates_a_write_into_the_memory_directory() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("annotated");
    let memory_file = guard
        .memory_path(&cwd, "user.md")
        .to_string_lossy()
        .into_owned();
    let notebook = guard
        .memory_path(&cwd, "notes.ipynb")
        .to_string_lossy()
        .into_owned();

    for (tool, input, path) in [
        ("Write", json!({"file_path": &memory_file}), &memory_file),
        ("Edit", json!({"file_path": &memory_file}), &memory_file),
        (
            "MultiEdit",
            json!({"file_path": &memory_file}),
            &memory_file,
        ),
        (
            "NotebookEdit",
            json!({"notebook_path": &notebook}),
            &notebook,
        ),
    ] {
        let out = annotate(&booted, tool, input, &cwd);
        assert_eq!(
            out[MEMORY_NOTIFICATION_FIELD],
            json!(expected_line(path, &cwd)),
            "{tool}"
        );
        assert_eq!(out["type"], "update", "{tool}: the tool's fields survive");
    }
}

/// A write outside the memory directories is untouched.
#[test]
fn an_ordinary_write_gets_no_line() {
    let guard = HomeGuard::new();
    let booted = boot();
    let cwd = guard.cwd("ordinary");
    let out = annotate(
        &booted,
        "Write",
        json!({"file_path": format!("{cwd}/src/lib.rs")}),
        &cwd,
    );
    assert_eq!(out, json!({"type": "update"}));
}

/// The subscriber is an effect of the plugin's context: switching the plugin
/// off takes it off the seat, and back on puts it back.
#[test]
fn the_switch_takes_the_subscriber_off_the_seat_and_puts_it_back() {
    let booted = boot();
    let seat = booted.turn_hook_seat();
    let id = rebon_plugin_memory::update_hook::HOOK_ID.to_string();

    assert!(seat.subscriber_ids().contains(&id));
    booted.set_enabled(false);
    assert!(!seat.subscriber_ids().contains(&id));
    booted.set_enabled(true);
    assert!(seat.subscriber_ids().contains(&id));
}
