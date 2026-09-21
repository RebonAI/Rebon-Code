//! The shape of a session id — the one place that knows what a session id
//! looks like.
//!
//! A fresh id is twenty lowercase alphanumerics in four hyphen-separated
//! groups of five, `xxxxx-xxxxx-xxxxx-xxxxx`, drawn from the operating
//! system's entropy source. Twenty base-36 characters carry about 103 bits,
//! so ids do not collide and none of them says anything about when or where
//! it was minted.
//!
//! An id is a name and nothing more. Ids in the shape minted before this one
//! (`sess-` and two hexadecimal fields) are still on disk and still open, and
//! nothing here has to recognize them: every reader treats an id as an opaque
//! string, and when a session was created is session data — recorded in its
//! metadata sidecar and on the first row of its transcript, never in its
//! name.

/// Characters an id is drawn from: the digits and the lowercase letters.
const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Groups in an id, and characters per group.
const GROUPS: usize = 4;
const GROUP_LEN: usize = 5;

/// Highest random byte that can be folded into the alphabet without bias:
/// 36 × 7 = 252, so bytes 252..=255 are drawn again rather than skewing the
/// first four characters of the alphabet.
const LARGEST_UNBIASED_BYTE: u8 = 252;

/// A fresh session id.
///
/// Public because a session can exist before any session table holds a
/// record for it: a terminal that starts a worker for a brand-new session
/// names the session first and builds its own state afterwards, and the two
/// must agree on the id.
///
/// # Panics
///
/// Panics if the operating system has no entropy to give. An id that is not
/// random is an id that collides with another session's transcript, so there
/// is nothing useful to return instead.
pub fn new_session_id() -> String {
    let mut id = String::with_capacity(GROUPS * GROUP_LEN + GROUPS - 1);
    // One buffer refilled as it is consumed, so a rejected byte costs a step
    // through the pool rather than a syscall.
    let mut pool = [0u8; 32];
    let mut used = pool.len();
    for group in 0..GROUPS {
        if group > 0 {
            id.push('-');
        }
        for _ in 0..GROUP_LEN {
            loop {
                if used == pool.len() {
                    getrandom::getrandom(&mut pool)
                        .expect("getrandom: kernel entropy source unavailable");
                    used = 0;
                }
                let byte = pool[used];
                used += 1;
                if byte < LARGEST_UNBIASED_BYTE {
                    id.push(ALPHABET[usize::from(byte % 36)] as char);
                    break;
                }
            }
        }
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_fresh_id_is_four_groups_of_five_lowercase_alphanumerics() {
        for _ in 0..1_000 {
            let id = new_session_id();
            assert_eq!(id.len(), 23, "{id} is not 20 characters plus 3 hyphens");
            let groups: Vec<&str> = id.split('-').collect();
            assert_eq!(groups.len(), 4, "{id} is not four groups");
            for group in groups {
                assert_eq!(group.len(), 5, "group {group} of {id} is not five long");
                assert!(
                    group
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase()),
                    "group {group} of {id} is outside [0-9a-z]"
                );
            }
        }
    }

    #[test]
    fn ten_thousand_fresh_ids_are_all_different() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(new_session_id()), "a session id repeated");
        }
    }

    #[test]
    fn every_alphabet_character_can_be_drawn() {
        let mut seen = HashSet::new();
        for _ in 0..5_000 {
            seen.extend(new_session_id().chars().filter(|c| *c != '-'));
        }
        for expected in ALPHABET.iter().map(|byte| char::from(*byte)) {
            assert!(seen.contains(&expected), "{expected} was never drawn");
        }
    }

    /// The clock is the thing an id must not leak. Two ids minted a
    /// measurable time apart share no prefix, no ordering and no digits an
    /// observer could read a date out of.
    #[test]
    fn ids_minted_at_different_times_have_nothing_in_common() {
        let first = new_session_id();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = new_session_id();
        assert_ne!(first, second);
        let shared = first
            .chars()
            .zip(second.chars())
            .take_while(|(a, b)| a == b)
            .count();
        assert!(
            shared < 5,
            "{first} and {second} share a {shared}-character prefix"
        );
    }
}
