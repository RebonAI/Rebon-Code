//! The sandbox accounts' passwords.
//!
//! These are the credentials for two real local accounts on the user's machine,
//! living for as long as Rebon is installed. `BCryptGenRandom` with the
//! system-preferred RNG, not a PRNG seeded from the clock: a predictable
//! password on a real account is a real weakness, whoever the account is.
//!
//! Generated in one place so the account and the credential store cannot
//! disagree about what was set — the store is the only copy, and a mismatch is
//! an account nobody can log into and no way to find out why.

use crate::sys::SysResult;

/// The length of a generated password.
pub const PASSWORD_LENGTH: usize = 32;

/// Pinned at compile time: this is the length of a credential on a real account,
/// and shortening it is the sort of edit that looks harmless.
const _: () = assert!(PASSWORD_LENGTH >= 32);

/// The alphabet passwords are drawn from.
///
/// Deliberately excludes the characters a Windows password policy or a command
/// line is liable to object to, and the visually ambiguous ones — this may end up
/// in front of a person recovering a machine by hand.
const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789!@#$%^&*()-_=+";

/// A fresh password.
pub fn password() -> SysResult<String> {
    let bytes = bytes(PASSWORD_LENGTH * 2)?;
    Ok(from_entropy(&bytes, PASSWORD_LENGTH))
}

/// Cryptographically random bytes.
pub fn bytes(count: usize) -> SysResult<Vec<u8>> {
    imp::bytes(count)
}

/// Map entropy onto the alphabet.
///
/// Split out and tested because the mapping is where an off-by-one silently
/// shrinks the keyspace, and no amount of good entropy fixes that.
fn from_entropy(entropy: &[u8], length: usize) -> String {
    entropy
        .iter()
        .take(length)
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(windows)]
mod imp {
    use crate::sys::{SysError, SysResult};
    use std::ptr::null_mut;
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };

    pub fn bytes(count: usize) -> SysResult<Vec<u8>> {
        let mut buffer = vec![0u8; count];
        let status = unsafe {
            BCryptGenRandom(
                null_mut(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(SysError::win32("BCryptGenRandom", status as u32));
        }
        Ok(buffer)
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::sys::{SysError, SysResult};

    pub fn bytes(_count: usize) -> SysResult<Vec<u8>> {
        Err(SysError::Unsupported("BCryptGenRandom"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alphabet_has_no_duplicates() {
        // A repeated character biases the mapping towards it, quietly.
        let mut sorted = ALPHABET.to_vec();
        let before = sorted.len();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), before);
    }

    #[test]
    fn the_alphabet_avoids_the_ambiguous_characters() {
        // This can end up in front of a person recovering a machine by hand.
        for ambiguous in [b'l', b'I', b'O', b'0', b'1'] {
            assert!(
                !ALPHABET.contains(&ambiguous),
                "{} is ambiguous",
                ambiguous as char
            );
        }
    }

    #[test]
    fn the_mapping_produces_the_requested_length() {
        // An off-by-one here silently shortens every password, and good entropy does not
        // fix a short one.
        let entropy: Vec<u8> = (0..=255).collect();
        assert_eq!(
            from_entropy(&entropy, PASSWORD_LENGTH).len(),
            PASSWORD_LENGTH
        );
        assert_eq!(from_entropy(&entropy, 1).len(), 1);
    }

    #[test]
    fn the_mapping_only_emits_alphabet_characters() {
        let entropy: Vec<u8> = (0..=255).collect();
        for character in from_entropy(&entropy, 200).bytes() {
            assert!(ALPHABET.contains(&character), "{character} is off-alphabet");
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn two_passwords_are_not_the_same() {
        let first = password().unwrap();
        let second = password().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), PASSWORD_LENGTH);
    }

    #[test]
    fn passwords_do_not_repeat_across_many_draws() {
        // Catches a generator that returns a constant, which is the way this fails in
        // practice — an ignored error code and a zeroed buffer.
        let drawn: HashSet<String> = (0..64).map(|_| password().unwrap()).collect();
        assert_eq!(drawn.len(), 64);
    }

    #[test]
    fn the_bytes_are_not_all_zero() {
        let drawn = bytes(64).unwrap();
        assert_eq!(drawn.len(), 64);
        assert!(drawn.iter().any(|byte| *byte != 0));
    }
}
