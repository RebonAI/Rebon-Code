//! Where a session's scratchpad lives, and deleting it once the session is
//! over.
//!
//! Here rather than beside the prompt that names it because two sides need
//! the one path: the engine that tells the model about it (`rebon-core`) and
//! the job store that ends a hosted session by removing its job
//! (`rebon-session-host`), which cannot depend on the engine.

use std::path::PathBuf;

use crate::session_storage::project_dir_component;

/// Resolve the scratchpad directory for a (project cwd, session key)
/// pair: `$REBON_TMPDIR/rebon/<sanitised-cwd>/<key>/scratchpad`.
///
/// The same path function backs both the coordinator session prompt
/// and sub-agent prompts, so a worker keyed by its parent session id
/// lands in the parent's scratchpad and the parent can inspect
/// whatever artifacts the worker leaves behind.
pub fn scratchpad_dir_for(cwd: &str, session_key: &str) -> String {
    let base_tmp_dir = std::env::var_os("REBON_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base_tmp_dir
        .join("rebon")
        .join(project_dir_component(cwd))
        .join(session_key)
        .join("scratchpad")
        .display()
        .to_string()
}

/// Delete the scratchpad [`scratchpad_dir_for`] resolves, once the session
/// it belongs to is over.
///
/// Called by whoever ends the session — never by a mirror, whose worker
/// still has the session and its files. Best effort: a file held open (on
/// Windows) or a permission error is logged and the rest of the teardown
/// carries on. The session and project directories above it exist only to
/// hold scratchpads, so they go too when that leaves them empty;
/// `remove_dir` refuses a non-empty one, which is what keeps another live
/// session's scratchpad in the same project untouched.
pub fn remove_scratchpad_for(cwd: &str, session_key: &str) {
    // A key that is not one path component would resolve somewhere other
    // than this session's own directory.
    if session_key.is_empty()
        || session_key == "."
        || session_key == ".."
        || session_key.contains(['/', '\\'])
    {
        return;
    }
    let scratchpad = PathBuf::from(scratchpad_dir_for(cwd, session_key));
    match std::fs::remove_dir_all(&scratchpad) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                path = %scratchpad.display(),
                %error,
                "could not remove the session scratchpad directory"
            );
            return;
        }
    }
    let Some(session_dir) = scratchpad.parent() else {
        return;
    };
    if std::fs::remove_dir(session_dir).is_ok() {
        if let Some(project_dir) = session_dir.parent() {
            let _ = std::fs::remove_dir(project_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratchpad_dir_for_is_keyed_by_project_and_session() {
        let dir = scratchpad_dir_for("F:/dev/proj", "sess-1");
        let normalized = dir.replace('\\', "/");
        assert!(normalized.contains("/rebon/"));
        assert!(normalized.contains("sess-1"));
        assert!(normalized.ends_with("/scratchpad"));
        // Same inputs, same path — parent and worker must agree.
        assert_eq!(dir, scratchpad_dir_for("F:/dev/proj", "sess-1"));
        assert_ne!(dir, scratchpad_dir_for("F:/dev/proj", "sess-2"));
    }

    #[test]
    fn remove_scratchpad_for_deletes_only_the_ended_sessions_directory() {
        // A cwd no other test uses, so the project directory under the real
        // temp root is this test's alone.
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().display().to_string();
        let ended = PathBuf::from(scratchpad_dir_for(&cwd, "sess-ended"));
        let live = PathBuf::from(scratchpad_dir_for(&cwd, "sess-live"));
        std::fs::create_dir_all(ended.join("nested")).unwrap();
        std::fs::write(ended.join("nested").join("out.txt"), "x").unwrap();
        std::fs::create_dir_all(&live).unwrap();

        remove_scratchpad_for(&cwd, "sess-ended");
        assert!(
            !ended.parent().unwrap().exists(),
            "session directory removed"
        );
        assert!(live.is_dir(), "a sibling session's scratchpad survives");

        remove_scratchpad_for(&cwd, "sess-live");
        let project_dir = live.parent().unwrap().parent().unwrap();
        assert!(
            !project_dir.exists(),
            "an emptied project directory goes too"
        );

        // Already gone, or never created: nothing to do, nothing to report.
        remove_scratchpad_for(&cwd, "sess-live");
    }

    #[test]
    fn remove_scratchpad_for_ignores_keys_that_are_not_one_component() {
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().display().to_string();
        let other = PathBuf::from(scratchpad_dir_for(&cwd, "sess-other"));
        std::fs::create_dir_all(&other).unwrap();
        for key in ["", ".", "..", "../sess-other", "a/b", "a\\b"] {
            remove_scratchpad_for(&cwd, key);
        }
        assert!(other.is_dir());
        remove_scratchpad_for(&cwd, "sess-other");
    }
}
