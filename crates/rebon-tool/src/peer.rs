//! Naming another session, once.
//!
//! Three spellings reach the same session: its handle (`@repo-qrst`), its id
//! (`session:abcde-fghij-klmno-pqrst`, or a prefix of it), and the name its job
//! was given. Every tool that addresses a peer goes through [`resolve_peer`] so
//! the three cannot drift into three different matching rules.
//!
//! The order is most specific first, and the first step that matches anything
//! decides: a handle, then a handle prefix, then an id, then a name. A step that
//! matches several sessions is an error rather than a reason to fall through —
//! continuing would deliver the message to a session the caller did not name.

use crate::PeerInfo;

/// How much of a session id or handle a caller may abbreviate. Short enough to
/// type, long enough that two sessions rarely share it.
pub const MIN_SESSION_PREFIX_CHARS: usize = 6;

/// Why [`resolve_peer`] could not name exactly one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerResolveError {
    /// No running session answers to this at all.
    NotFound { requested: String },
    /// Several do; each candidate is spelled the way a caller should send to it.
    Ambiguous {
        requested: String,
        candidates: Vec<String>,
    },
}

impl PeerResolveError {
    /// The candidate list, spelled `@handle` where there is one and
    /// `session:<id>` otherwise.
    pub fn candidates(&self) -> &[String] {
        match self {
            Self::NotFound { .. } => &[],
            Self::Ambiguous { candidates, .. } => candidates,
        }
    }
}

impl std::fmt::Display for PeerResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { requested } => {
                write!(f, "no running session matches `{requested}`")
            }
            Self::Ambiguous {
                requested,
                candidates,
            } => write!(
                f,
                "`{requested}` matches {} sessions: {}",
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }
}

/// A session spelled the way a caller should write it.
pub fn peer_target_label(peer: &PeerInfo) -> String {
    match peer.handle.as_deref().map(str::trim) {
        Some(handle) if !handle.is_empty() => format!("@{handle}"),
        _ => format!("session:{}", peer.session_id),
    }
}

/// Strip one leading `@`, the sigil a handle is written with.
pub fn strip_handle_sigil(requested: &str) -> &str {
    requested.strip_prefix('@').unwrap_or(requested)
}

/// The one session `requested` names.
///
/// A leading `@` is accepted and ignored, so the same string works in a tool
/// argument and in what a person types at the prompt.
pub fn resolve_peer<'a>(
    peers: &'a [PeerInfo],
    requested: &str,
) -> Result<&'a PeerInfo, PeerResolveError> {
    let requested = strip_handle_sigil(requested).trim();
    let not_found = || PeerResolveError::NotFound {
        requested: requested.to_string(),
    };
    if requested.is_empty() {
        return Err(not_found());
    }

    let by_handle: Vec<&PeerInfo> = peers
        .iter()
        .filter(|peer| {
            peer.handle
                .as_deref()
                .is_some_and(|handle| handle.eq_ignore_ascii_case(requested))
        })
        .collect();
    if let Some(peer) = resolve_step(by_handle, requested)? {
        return Ok(peer);
    }

    if requested.len() >= MIN_SESSION_PREFIX_CHARS {
        let by_handle_prefix: Vec<&PeerInfo> = peers
            .iter()
            .filter(|peer| {
                peer.handle.as_deref().is_some_and(|handle| {
                    handle
                        .to_ascii_lowercase()
                        .starts_with(&requested.to_ascii_lowercase())
                })
            })
            .collect();
        if let Some(peer) = resolve_step(by_handle_prefix, requested)? {
            return Ok(peer);
        }

        if let Some(peer) = peers.iter().find(|peer| peer.session_id == requested) {
            return Ok(peer);
        }
        let by_id_prefix: Vec<&PeerInfo> = peers
            .iter()
            .filter(|peer| peer.session_id.starts_with(requested))
            .collect();
        if let Some(peer) = resolve_step(by_id_prefix, requested)? {
            return Ok(peer);
        }
    }

    let by_name: Vec<&PeerInfo> = peers
        .iter()
        .filter(|peer| {
            peer.name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(requested))
        })
        .collect();
    resolve_step(by_name, requested)?.ok_or_else(not_found)
}

/// One step of [`resolve_peer`]: a unique hit ends the search, several are an
/// ambiguity, none passes the decision to the next step.
fn resolve_step<'a>(
    matches: Vec<&'a PeerInfo>,
    requested: &str,
) -> Result<Option<&'a PeerInfo>, PeerResolveError> {
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => Err(PeerResolveError::Ambiguous {
            requested: requested.to_string(),
            candidates: matches.into_iter().map(peer_target_label).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(session_id: &str, handle: Option<&str>, name: Option<&str>) -> PeerInfo {
        PeerInfo {
            session_id: session_id.into(),
            handle: handle.map(str::to_string),
            name: name.map(str::to_string),
            cwd: "C:/work".into(),
            kind: "hosted".into(),
            busy: false,
            reachable: true,
            terminal_id: None,
        }
    }

    fn peers() -> Vec<PeerInfo> {
        vec![
            peer("abcde-fghij-klmno-pqrst", Some("repo-qrst"), Some("worker")),
            peer("abcde-fghij-klmno-zzzz", Some("repo-zzzz"), Some("other")),
            peer("11111-22222-33333-44444", None, None),
        ]
    }

    #[test]
    fn a_full_handle_matches_whatever_its_case() {
        let peers = peers();
        for requested in ["repo-qrst", "@repo-qrst", "@REPO-QRST"] {
            let resolved = resolve_peer(&peers, requested).unwrap();
            assert_eq!(
                resolved.session_id, "abcde-fghij-klmno-pqrst",
                "{requested}"
            );
        }
    }

    #[test]
    fn a_handle_prefix_of_at_least_six_characters_matches() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "@repo-qr").unwrap().session_id,
            "abcde-fghij-klmno-pqrst"
        );
    }

    #[test]
    fn a_handle_prefix_shorter_than_six_characters_does_not_reach_the_handle_step() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "@repo"),
            Err(PeerResolveError::NotFound {
                requested: "repo".into()
            })
        );
    }

    #[test]
    fn a_full_session_id_and_a_unique_prefix_both_match() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "abcde-fghij-klmno-pqrst")
                .unwrap()
                .session_id,
            "abcde-fghij-klmno-pqrst"
        );
        assert_eq!(
            resolve_peer(&peers, "abcde-fghij-klmno-pq")
                .unwrap()
                .session_id,
            "abcde-fghij-klmno-pqrst"
        );
        assert_eq!(
            resolve_peer(&peers, "11111-22222-33333-44444")
                .unwrap()
                .session_id,
            "11111-22222-33333-44444"
        );
    }

    #[test]
    fn a_name_matches_whatever_its_case() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "Other").unwrap().session_id,
            "abcde-fghij-klmno-zzzz"
        );
        assert_eq!(
            resolve_peer(&peers, "11111-22222-33333-44444")
                .unwrap()
                .session_id,
            "11111-22222-33333-44444"
        );
    }

    #[test]
    fn the_handle_step_wins_over_a_name_that_spells_the_same_thing() {
        let peers = vec![
            peer("abcde-fghij-klmno-qqqq", Some("lead"), None),
            peer("abcde-fghij-klmno-wwww", None, Some("lead")),
        ];
        assert_eq!(
            resolve_peer(&peers, "lead").unwrap().session_id,
            "abcde-fghij-klmno-qqqq"
        );
    }

    #[test]
    fn an_ambiguous_step_lists_the_candidates_and_stops() {
        let peers = vec![
            peer("abcde-one", Some("repo-one"), Some("worker")),
            peer("abcde-two", Some("repo-only"), Some("worker")),
        ];
        let error = resolve_peer(&peers, "@repo-o").unwrap_err();
        assert_eq!(
            error,
            PeerResolveError::Ambiguous {
                requested: "repo-o".into(),
                candidates: vec!["@repo-one".into(), "@repo-only".into()],
            }
        );
        assert!(
            error.to_string().contains("@repo-one, @repo-only"),
            "{error}"
        );

        let error = resolve_peer(&peers, "worker").unwrap_err();
        assert_eq!(error.candidates(), ["@repo-one", "@repo-only"]);
    }

    #[test]
    fn an_ambiguous_id_prefix_lists_ids_when_the_peers_have_no_handle() {
        let peers = vec![
            peer("abcdef-one", None, None),
            peer("abcdef-two", None, None),
        ];
        let error = resolve_peer(&peers, "abcdef").unwrap_err();
        assert_eq!(
            error.candidates(),
            ["session:abcdef-one", "session:abcdef-two"]
        );
    }

    #[test]
    fn nothing_that_matches_reports_not_found() {
        let peers = peers();
        assert_eq!(
            resolve_peer(&peers, "ghost"),
            Err(PeerResolveError::NotFound {
                requested: "ghost".into()
            })
        );
        assert_eq!(
            resolve_peer(&peers, ""),
            Err(PeerResolveError::NotFound {
                requested: "".into()
            })
        );
    }

    #[test]
    fn a_candidate_is_spelled_with_its_handle_when_it_has_one() {
        assert_eq!(
            peer_target_label(&peer("abcde-fghij-klmno-pqrst", Some("repo-qrst"), None)),
            "@repo-qrst"
        );
        assert_eq!(
            peer_target_label(&peer("abcde-fghij-klmno-pqrst", None, None)),
            "session:abcde-fghij-klmno-pqrst"
        );
    }
}
