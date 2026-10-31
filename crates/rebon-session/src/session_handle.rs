//! The short, typeable name a session goes by when another session addresses
//! it: `<project-slug>-<tail>`.
//!
//! A session id is twenty random characters long and says nothing a person can
//! remember, while the two facts that tell two sessions apart at a glance are
//! the project they run in and the tail of their id. A handle is exactly those
//! two, and nothing else: it is derived, not chosen, so `/rename` cannot move a
//! peer's target out from under a sender.
//!
//! A handle has to be unique among every session on this machine that has ever
//! held one, not just among the sessions running now — a sender may be naming a
//! session it expects to come back. The claim is therefore a file in a
//! machine-wide index, created with `create_new` so two processes racing for the
//! same handle cannot both win: the loser lengthens its id tail and tries
//! again.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Longest the project half of a handle may be.
pub const SESSION_HANDLE_SLUG_CHARS: usize = 24;

/// How much of a session id's tail the first attempt takes.
pub const SESSION_HANDLE_TAIL_CHARS: usize = 4;

/// Where the machine-wide claim index lives, under the projects root.
pub const SESSION_HANDLE_INDEX_DIR: &str = "handles";

/// Fallback slug for a cwd with no usable directory name — the filesystem root.
const FALLBACK_SLUG: &str = "session";

/// The project half of a handle: the cwd's last directory name.
///
/// A worktree is named after the project it belongs to rather than after the
/// worktree directory, because `.worktrees` is a directory every worktree of
/// one project sits in and would otherwise make them all look alike.
pub fn handle_project_slug(cwd: &str) -> String {
    let normalized = cwd.replace('\\', "/");
    let segments = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let base = match segments.iter().position(|segment| *segment == ".worktrees") {
        Some(index) if index > 0 => segments[index - 1],
        Some(_) => segments.last().copied().unwrap_or_default(),
        None => segments.last().copied().unwrap_or_default(),
    };
    slugify(base)
}

fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let truncated = trimmed
        .chars()
        .take(SESSION_HANDLE_SLUG_CHARS)
        .collect::<String>();
    let truncated = truncated.trim_end_matches('-');
    if truncated.is_empty() {
        FALLBACK_SLUG.to_string()
    } else {
        truncated.to_string()
    }
}

/// The session half of a handle, before it is shortened to a suffix: the
/// session id without its group separators, lowercased.
fn handle_tail(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|ch| *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// `<slug>-<last n characters of the id>`, or `None` when the id is too short to
/// give `extra_chars` more characters than the starting suffix.
pub fn session_handle_candidate(cwd: &str, session_id: &str, extra_chars: usize) -> Option<String> {
    let slug = handle_project_slug(cwd);
    session_handle_for_slug(&slug, session_id, extra_chars)
}

fn session_handle_for_slug(slug: &str, session_id: &str, extra_chars: usize) -> Option<String> {
    let tail = handle_tail(session_id);
    let take = SESSION_HANDLE_TAIL_CHARS + extra_chars;
    if tail.len() < take {
        return None;
    }
    let start = tail.len() - take;
    Some(format!("{slug}-{}", &tail[start..]))
}

/// The handle this session's metadata sidecar records, if any.
///
/// Read-only on purpose: a session that has never been claimed has no handle,
/// and only the owner — the process holding its lock — may mint one.
pub fn load_session_handle(projects_root: &Path, cwd: &str, session_id: &str) -> Option<String> {
    let path = crate::session_storage::session_meta_path(projects_root, cwd, session_id);
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("handle")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|handle| !handle.is_empty())
        .map(str::to_string)
}

/// The handle this session goes by, minting and claiming one if it has none.
///
/// Idempotent: a session that already records a handle gets it back unchanged,
/// so a later call — a resume, a worker rebuilding its session — never moves
/// the name other sessions learned. The claim and the sidecar write happen in
/// that order, so a crash between them leaves a claim that a later call finds
/// naming this session and finishes.
pub fn ensure_session_handle(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> std::io::Result<String> {
    if let Some(existing) = load_session_handle(projects_root, cwd, session_id) {
        return Ok(existing);
    }
    if session_id.trim().is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "a session id is needed to mint a handle",
        ));
    }
    let slug = handle_project_slug(cwd);
    let mut extra_chars = 0usize;
    loop {
        let Some(candidate) = session_handle_for_slug(&slug, session_id, extra_chars) else {
            return Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                format!(
                    "no unused handle fits session {session_id}: every suffix of its id is claimed"
                ),
            ));
        };
        if claim_handle(projects_root, &candidate, session_id)? {
            crate::session_storage::update_session_metadata(
                projects_root,
                cwd,
                session_id,
                |meta| {
                    meta.insert(
                        "handle".into(),
                        serde_json::Value::String(candidate.clone()),
                    );
                },
            )?;
            return Ok(candidate);
        }
        extra_chars += 1;
    }
}

/// Claim `handle` for `session_id`, or report that somebody else already did.
///
/// `create_new` is the whole mutual exclusion: a name that appears is one
/// exactly one process created. A file that already names this session is this
/// session's own earlier claim — the sidecar write after it must have failed —
/// rather than a conflict.
fn claim_handle(projects_root: &Path, handle: &str, session_id: &str) -> std::io::Result<bool> {
    let dir = projects_root.join(SESSION_HANDLE_INDEX_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = handle_index_path(&dir, handle);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(session_id.as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let owner = std::fs::read_to_string(&path).unwrap_or_default();
            Ok(owner.trim() == session_id)
        }
        Err(error) => Err(error),
    }
}

fn handle_index_path(index_dir: &Path, handle: &str) -> PathBuf {
    index_dir.join(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_the_last_directory_name_lowercased() {
        assert_eq!(handle_project_slug("/repo"), "repo");
        assert_eq!(handle_project_slug("/Users/bon/Dev/RebonCode"), "reboncode");
        assert_eq!(handle_project_slug("F:\\dev\\Work"), "work");
        assert_eq!(handle_project_slug("/repo/"), "repo");
    }

    #[test]
    fn a_worktree_is_named_after_the_project_it_belongs_to() {
        assert_eq!(
            handle_project_slug("/Users/bon/Dev/reboncode/.worktrees/cross-session"),
            "reboncode"
        );
        assert_eq!(
            handle_project_slug("/Users/bon/Dev/reboncode/.worktrees/cross-session/"),
            "reboncode"
        );
    }

    #[test]
    fn characters_outside_the_alphabet_collapse_to_one_dash() {
        assert_eq!(handle_project_slug("/tmp/my  project!!"), "my-project");
        assert_eq!(handle_project_slug("/tmp/-Lead_in-"), "lead-in");
    }

    #[test]
    fn a_slug_longer_than_the_limit_is_truncated_without_a_trailing_dash() {
        let long = format!("/tmp/{}", "abcdefghij".repeat(4));
        let slug = handle_project_slug(&long);
        assert_eq!(slug.len(), SESSION_HANDLE_SLUG_CHARS);
        assert_eq!(slug, "abcdefghijabcdefghijabcd");

        let trimmed = handle_project_slug("/tmp/abcdefghijabcdefghijabc/def");
        assert!(!trimmed.ends_with('-'), "{trimmed}");
    }

    #[test]
    fn a_cwd_with_no_name_falls_back_to_session() {
        assert_eq!(handle_project_slug("/"), FALLBACK_SLUG);
        assert_eq!(handle_project_slug(""), FALLBACK_SLUG);
        assert_eq!(handle_project_slug("///"), FALLBACK_SLUG);
        assert_eq!(handle_project_slug("/tmp/!!!"), FALLBACK_SLUG);
    }

    #[test]
    fn a_handle_is_the_slug_and_the_last_four_id_characters() {
        assert_eq!(
            session_handle_candidate("/repo", "abcde-fghij-klmno-pqrst", 0).as_deref(),
            Some("repo-qrst")
        );
        assert_eq!(
            session_handle_candidate("/repo", "abcde-fghij-klmno-pqrst", 1).as_deref(),
            Some("repo-pqrst")
        );
    }

    #[test]
    fn an_id_too_short_for_another_character_yields_no_candidate() {
        assert_eq!(session_handle_candidate("/repo", "abc", 0), None);
        assert_eq!(
            session_handle_candidate("/repo", "abc-d", 0).as_deref(),
            Some("repo-abcd")
        );
        assert_eq!(session_handle_candidate("/repo", "abc-d", 1), None);
        assert_eq!(session_handle_candidate("/repo", "", 0), None);
    }

    #[test]
    fn a_handle_uses_only_the_characters_a_recipient_name_may_contain() {
        for cwd in ["/repo", "/tmp/My Project", "/", "/a/b-c/d_e.f"] {
            for extra in 0..18 {
                let Some(handle) = session_handle_candidate(cwd, "0123456789abcdefghij", extra)
                else {
                    continue;
                };
                assert!(
                    handle
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
                    "{handle} is outside [a-z0-9-]"
                );
                assert!(
                    !handle.starts_with('-') && !handle.ends_with('-'),
                    "{handle}"
                );
            }
        }
    }

    fn meta_path(root: &Path, cwd: &str, session_id: &str) -> PathBuf {
        crate::session_storage::session_meta_path(root, cwd, session_id)
    }

    #[test]
    fn the_first_session_of_a_project_gets_the_four_character_tail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let handle = ensure_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").unwrap();
        assert_eq!(handle, "repo-qrst");
        assert_eq!(
            load_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").as_deref(),
            Some("repo-qrst")
        );
        assert_eq!(
            std::fs::read_to_string(root.join(SESSION_HANDLE_INDEX_DIR).join("repo-qrst")).unwrap(),
            "abcde-fghij-klmno-pqrst"
        );
    }

    #[test]
    fn a_taken_handle_lengthens_the_tail_until_it_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let first = ensure_session_handle(root, "/repo", "aaaaa-bbbbb-ccccc-qrst").unwrap();
        let second = ensure_session_handle(root, "/repo", "zzzzz-yyyyy-xxxxx-qrst").unwrap();
        assert_eq!(first, "repo-qrst");
        assert_eq!(second, "repo-xqrst");
    }

    /// Every suffix this id can offer is already somebody else's, so the session
    /// is refused rather than handed a name that points at another session.
    #[test]
    fn an_id_with_no_room_left_reports_that_every_suffix_is_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let index = root.join(SESSION_HANDLE_INDEX_DIR);
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(index.join("repo-abcd"), "someone-else").unwrap();

        let error = ensure_session_handle(root, "/repo", "abcd").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("every suffix"), "{error}");
        assert_eq!(load_session_handle(root, "/repo", "abcd"), None);
    }

    #[test]
    fn ensuring_twice_returns_the_same_handle_without_touching_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let first = ensure_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").unwrap();
        let second = ensure_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").unwrap();
        assert_eq!(first, second);
        let index = root.join(SESSION_HANDLE_INDEX_DIR);
        assert_eq!(std::fs::read_dir(&index).unwrap().count(), 1);
    }

    /// Two processes racing for one handle: exactly one creates the index file,
    /// and the loser moves on to a longer tail rather than adopting the name.
    #[test]
    fn only_one_of_two_racing_sessions_keeps_the_contested_handle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let handles = std::thread::scope(|scope| {
            let one =
                scope.spawn(|| ensure_session_handle(&root, "/repo", "aaaaa-bbbbb-ccccc-qrst"));
            let two =
                scope.spawn(|| ensure_session_handle(&root, "/repo", "zzzzz-yyyyy-xxxxx-qrst"));
            vec![one.join().unwrap().unwrap(), two.join().unwrap().unwrap()]
        });
        let mut sorted = handles.clone();
        sorted.sort();
        assert_eq!(sorted.len(), 2);
        assert_ne!(sorted[0], sorted[1], "{sorted:?}");
        assert!(sorted.contains(&"repo-qrst".to_string()), "{sorted:?}");
        assert_eq!(
            std::fs::read_dir(root.join(SESSION_HANDLE_INDEX_DIR))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn a_handle_is_recorded_beside_the_title_and_the_agent_choice() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::session_storage::save_session_title(
            root,
            "/repo",
            "abcde-fghij-klmno-pqrst",
            "Fix login",
        )
        .unwrap();
        crate::session_storage::save_session_agent(
            root,
            "/repo",
            "abcde-fghij-klmno-pqrst",
            "local",
        )
        .unwrap();
        ensure_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").unwrap();

        let meta: serde_json::Value = serde_json::from_slice(
            &std::fs::read(meta_path(root, "/repo", "abcde-fghij-klmno-pqrst")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["title"], serde_json::json!("Fix login"));
        assert_eq!(meta["acpAgent"], serde_json::json!("local"));
        assert_eq!(meta["handle"], serde_json::json!("repo-qrst"));
    }

    #[test]
    fn a_title_written_after_the_handle_keeps_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        ensure_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").unwrap();
        crate::session_storage::save_session_title(
            root,
            "/repo",
            "abcde-fghij-klmno-pqrst",
            "Later",
        )
        .unwrap();
        assert_eq!(
            load_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").as_deref(),
            Some("repo-qrst")
        );
    }

    #[test]
    fn a_session_that_already_records_a_handle_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::session_storage::update_session_metadata(
            root,
            "/repo",
            "abcde-fghij-klmno-pqrst",
            |meta| {
                meta.insert("handle".into(), serde_json::json!("chosen-name"));
            },
        )
        .unwrap();
        assert_eq!(
            ensure_session_handle(root, "/repo", "abcde-fghij-klmno-pqrst").unwrap(),
            "chosen-name"
        );
        assert!(!root.join(SESSION_HANDLE_INDEX_DIR).exists());
    }

    #[test]
    fn a_session_with_no_sidecar_has_no_handle() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_session_handle(dir.path(), "/repo", "abcde-fghij-klmno-pqrst"),
            None
        );
    }
}
