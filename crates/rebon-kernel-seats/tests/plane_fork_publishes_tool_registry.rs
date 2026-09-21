//! The plane's fork has to be able to publish `tool-registry`, and doing so
//! must not take the root's seat away from anyone else.
//!
//! One word decides both. `Context::fork` shares the parent's service layer,
//! so a `provide_dual` on the plane's fork lands in the same layer where
//! `core-tools` already provides `tool-registry` and is refused as a
//! duplicate — silently, because the boot only logs the refusal. Every boot
//! carried "composition tool-registry failed to publish", and the composition's
//! tools were never on the seat at all. `fork_scoped` gives the child a layer
//! of its own, where the pair shadows the inherited name for that subtree and
//! vanishes with it.
//!
//! Needs no Node: what is being pinned is the kernel's visibility rule, which
//! is the whole of the bug.

use std::sync::Arc;

use rebon_core::tool_seat::{ToolSeat, ToolSeatService};
use rebon_kernel::Kernel;
use rebon_kernel_seats::kernel_compose_tools::ComposeToolRegistry;

/// A root that already has the typed seat, the way `core-tools` leaves it.
fn kernel_with_core_tools() -> (Arc<Kernel>, Arc<ToolSeat>) {
    let kernel = Kernel::new();
    let seat = ToolSeat::new();
    kernel
        .context()
        .provide::<ToolSeatService>(seat.clone())
        .expect("the root seat provides, as core-tools leaves it");
    (kernel, seat)
}

fn compose_registry() -> Arc<ComposeToolRegistry> {
    let registry = ComposeToolRegistry::new(Vec::<String>::new());
    registry.declare(
        "dsh_echo",
        "Echo, from the composition".to_string(),
        None,
        true,
    );
    registry
}

#[test]
fn a_scoped_plane_fork_publishes_both_faces_and_leaves_the_root_seat_standing() {
    let (kernel, root_seat) = kernel_with_core_tools();
    let registry = compose_registry();

    let plane = kernel.context().fork_scoped("plugin-plane");
    plane
        .provide_dual::<ToolSeatService>(root_seat.clone(), registry.clone())
        .expect("the plane's own layer takes the pair");

    // The JSON face, which is what a composition's tools are looked up
    // through, answers on the fork.
    let listed = plane
        .call_json("tool-registry", "list", serde_json::Value::Null)
        .expect("the composition's registry answers under the plane fork");
    let names: Vec<String> = listed["pluginTools"]
        .as_array()
        .expect("pluginTools is a list")
        .iter()
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(names, vec!["dsh_echo".to_string()]);

    // The typed face is the root's seat, not a second one: a plugin that
    // registers a tool under the plane still registers it where the engine
    // reads, which is the whole reason `provide_dual` is used here.
    let typed = plane
        .get::<ToolSeatService>()
        .expect("the typed seat resolves under the plane fork");
    assert!(
        Arc::ptr_eq(&typed, &root_seat),
        "the plane fork must see the root's seat, not a copy of it"
    );

    // And the root is untouched: a sibling context — every session is one —
    // still resolves the seat, and does not see the composition's JSON face.
    let sibling = kernel.context().fork("a-session");
    assert!(
        Arc::ptr_eq(
            &sibling
                .get::<ToolSeatService>()
                .expect("the root seat still resolves outside the plane"),
            &root_seat
        ),
        "publishing on the plane's layer must not shadow the root for anyone else"
    );
}

/// The bug itself, kept as a test so the one-word fix cannot be undone by
/// someone tidying `fork_scoped` back into `fork`.
#[test]
fn a_plain_fork_cannot_publish_tool_registry_at_all() {
    let (kernel, root_seat) = kernel_with_core_tools();
    let registry = compose_registry();

    let plane = kernel.context().fork("plugin-plane");
    let refused = plane.provide_dual::<ToolSeatService>(root_seat, registry);
    assert!(
        refused.is_err(),
        "a plain fork shares the root's layer, so this is a duplicate provider"
    );
}
