//! A plane boot that failed stays failed until something says otherwise.
//!
//! What this pins is a cost, not a message. `ensure_plane` used to clear the
//! standing refusal at the top of every attempt, so each caller that wanted a
//! plane on an unusable machine started another Node: one selected-but-broken
//! provider turned into a string of full boot attempts, each paying the
//! startup cost again and each leaving a host behind. On a real machine that
//! reads as rebon hanging for minutes at startup.
//!
//! Its own test binary because the plane slot, the refusal cell and the boot
//! counter are all process-wide, and this test is about their sequence.

use std::time::{Duration, Instant};

/// A Node that cannot be a Node, so the boot fails for a reason that needs no
/// network, no fixture and no waiting.
fn point_at_a_dead_end() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let fake = dir.path().join("not-node.exe");
    std::fs::write(&fake, b"this is not an executable").expect("fake node written");
    std::env::set_var("REBON_PLUGIN_NODE", &fake);
    std::env::set_var("REBON_CONFIG_DIR", dir.path());
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_plane_boot_is_not_retried_by_the_next_caller() {
    let _dir = point_at_a_dead_end();

    let before = rebon_plugin_host::plugin_boot::plane_boot_attempts();
    assert!(
        rebon_plugin_host::plugin_boot::ensure_plane_for_provider(
            &rebon_harness::kernel_bootstrap::process_plugin_registry(),
        )
        .await
        .is_none(),
        "a plane cannot start on a Node that is not one"
    );
    let after_first = rebon_plugin_host::plugin_boot::plane_boot_attempts();
    assert_eq!(
        after_first - before,
        1,
        "the first caller is the one that tries"
    );

    // The refusal the first attempt recorded is what a surface shows, and it
    // has to survive the second caller rather than being cleared by it.
    let refusal = rebon_plugin_host::plugin_boot::composition_refusal()
        .expect("a failed boot leaves a reason behind");
    assert!(!refusal.message().is_empty());

    // Three more callers, which is the shape of the real path: a resolution, a
    // session assembly and a status endpoint all wanting the same plane.
    let started = Instant::now();
    for _ in 0..3 {
        assert!(
            rebon_plugin_host::plugin_boot::ensure_plane_for_provider(
                &rebon_harness::kernel_bootstrap::process_plugin_registry(),
            )
            .await
            .is_none(),
            "the standing refusal answers without another attempt"
        );
    }
    assert_eq!(
        rebon_plugin_host::plugin_boot::plane_boot_attempts(),
        after_first,
        "no caller after the first one started another host"
    );
    // And they answered from the verdict rather than by doing the work again.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "three refusals took {:?}",
        started.elapsed()
    );

    std::env::remove_var("REBON_PLUGIN_NODE");
    std::env::remove_var("REBON_CONFIG_DIR");
}
