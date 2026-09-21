//! The sandbox accounts' passwords.
//!
//! `CreateProcessWithLogonW` takes a password, so the helper has to be able to
//! produce one without a human present. DPAPI in **user scope** is what keeps
//! that from being the same thing as leaving it on the disk: only the user who
//! ran `install` can decrypt it, and neither sandbox account is that user.
//!
//! User scope and not `CRYPTPROTECT_LOCAL_MACHINE` — the machine scope would let
//! any account on the box decrypt it, which includes the two we are confining.
//! On top of that the file's DACL denies the sandbox group outright
//! ([`crate::core::sddl::credentials_file_dacl`]), so the deny and the encryption
//! fail independently.
//!
//! That belt and braces is proportionate. A confined command that reads this
//! file can log on as `rebon-sbx` — the account with **no** WFP filters — and
//! `--block-network` stops meaning anything at all, silently.
//!
//! `credentials=ok` in `status` means "two non-empty passwords decrypted", not
//! "the file is there". A file that exists and cannot be decrypted (a different
//! user, a restored profile) is exactly the case where `install` needs
//! re-running, and reporting it as present would hide that.

use crate::sys::{SysError, SysResult};
use std::path::Path;

/// The passwords for both sandbox accounts.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub user_no_network: String,
}

impl Credentials {
    pub fn is_usable(&self) -> bool {
        !self.user.is_empty() && !self.user_no_network.is_empty()
    }
}

/// Redacted on purpose: these end up in `tracing` output and panic messages the
/// moment someone adds a `{:?}` while debugging, and a password in a log file is
/// a password on the disk in plaintext.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("user", &"<redacted>")
            .field("user_no_network", &"<redacted>")
            .finish()
    }
}

/// The on-disk form: two length-prefixed UTF-8 strings, then DPAPI over the
/// whole thing.
///
/// Hand-rolled rather than JSON so the plaintext never passes through a
/// serializer that might keep a copy of it in an intermediate buffer, and so the
/// format cannot grow a field by accident.
fn encode(credentials: &Credentials) -> Vec<u8> {
    let mut plain = Vec::new();
    for value in [&credentials.user, &credentials.user_no_network] {
        let bytes = value.as_bytes();
        plain.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        plain.extend_from_slice(bytes);
    }
    plain
}

fn decode(plain: &[u8]) -> SysResult<Credentials> {
    let mut cursor = 0usize;
    let mut values = Vec::with_capacity(2);
    for _ in 0..2 {
        if plain.len() < cursor + 4 {
            return Err(SysError::Invalid(
                "the credentials file is truncated — re-run `sandbox-win.exe install`".into(),
            ));
        }
        let length = u32::from_le_bytes([
            plain[cursor],
            plain[cursor + 1],
            plain[cursor + 2],
            plain[cursor + 3],
        ]) as usize;
        cursor += 4;
        if plain.len() < cursor + length {
            return Err(SysError::Invalid(
                "the credentials file is truncated — re-run `sandbox-win.exe install`".into(),
            ));
        }
        values.push(
            String::from_utf8(plain[cursor..cursor + length].to_vec())
                .map_err(|_| SysError::Invalid("the credentials file is not valid UTF-8".into()))?,
        );
        cursor += length;
    }
    Ok(Credentials {
        user: values.remove(0),
        user_no_network: values.remove(0),
    })
}

/// Encrypt and write. The caller is responsible for the file's DACL.
pub fn store(path: &Path, credentials: &Credentials) -> SysResult<()> {
    let sealed = imp::protect(&encode(credentials))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| SysError::io("creating the credentials directory", error))?;
    }
    std::fs::write(path, sealed)
        .map_err(|error| SysError::io("writing the credentials file", error))
}

/// Read and decrypt.
pub fn load(path: &Path) -> SysResult<Credentials> {
    let sealed =
        std::fs::read(path).map_err(|error| SysError::io("reading the credentials file", error))?;
    decode(&imp::unprotect(&sealed)?)
}

/// The `credentials=ok` probe.
///
/// Swallows every failure into `false` deliberately: the caller is `status`,
/// whose contract is a clean exit and three substrings, and the remedy is the
/// same sentence whichever way this went wrong.
pub fn are_usable(path: &Path) -> bool {
    load(path)
        .map(|credentials| credentials.is_usable())
        .unwrap_or(false)
}

#[cfg(windows)]
mod imp {
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(output.pbData as *mut c_void) };
        bytes
    }

    pub fn protect(plain: &[u8]) -> SysResult<Vec<u8>> {
        let input = blob(plain);
        let mut output: CRYPT_INTEGER_BLOB = unsafe { std::mem::zeroed() };
        // `dwFlags` is 0, not `CRYPTPROTECT_LOCAL_MACHINE`: machine scope would let the
        // sandbox accounts decrypt their own passwords.
        let ok =
            unsafe { CryptProtectData(&input, null(), null(), null_mut(), null(), 0, &mut output) };
        if ok == 0 {
            return Err(SysError::win32("CryptProtectData", unsafe {
                GetLastError()
            }));
        }
        Ok(take(output))
    }

    pub fn unprotect(sealed: &[u8]) -> SysResult<Vec<u8>> {
        let input = blob(sealed);
        let mut output: CRYPT_INTEGER_BLOB = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null_mut(),
                null(),
                0,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(SysError::win32("CryptUnprotectData", unsafe {
                GetLastError()
            }));
        }
        Ok(take(output))
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::sys::{SysError, SysResult};

    pub fn protect(_plain: &[u8]) -> SysResult<Vec<u8>> {
        Err(SysError::Unsupported("DPAPI"))
    }

    pub fn unprotect(_sealed: &[u8]) -> SysResult<Vec<u8>> {
        Err(SysError::Unsupported("DPAPI"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            user: "a-long-random-password-0123456789".into(),
            user_no_network: "another-long-random-password-9876".into(),
        }
    }

    #[test]
    fn the_encoding_round_trips() {
        assert_eq!(decode(&encode(&sample())).unwrap(), sample());
    }

    #[test]
    fn an_empty_password_round_trips_but_is_not_usable() {
        // A blank password would make `CreateProcessWithLogonW` fail with a logon error
        // that reads like a wrong password rather than like a broken install.
        let blank = Credentials {
            user: String::new(),
            user_no_network: "x".into(),
        };
        assert_eq!(decode(&encode(&blank)).unwrap(), blank);
        assert!(!blank.is_usable());
        assert!(sample().is_usable());
    }

    #[test]
    fn a_truncated_file_is_refused_with_the_remedy() {
        let encoded = encode(&sample());
        let error = decode(&encoded[..encoded.len() - 4]).unwrap_err();
        assert!(error.to_string().contains("install"), "{error}");
    }

    #[test]
    fn an_empty_file_is_refused() {
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn a_length_beyond_the_buffer_is_refused_rather_than_panicking() {
        // A corrupt length prefix is untrusted input from the disk; slicing on it
        // without a check would abort the helper.
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn passwords_are_redacted_in_debug_output() {
        // These reach `tracing` and panic messages the first time someone adds a `{:?}`
        // while debugging.
        let rendered = format!("{:?}", sample());
        assert!(!rendered.contains("a-long-random-password"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn a_missing_file_is_not_usable() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!are_usable(&temp.path().join("credentials.bin")));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            user: "a-long-random-password-0123456789".into(),
            user_no_network: "another-long-random-password-9876".into(),
        }
    }

    #[test]
    fn credentials_survive_a_trip_through_dpapi_and_the_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.bin");

        store(&path, &sample()).unwrap();

        assert_eq!(load(&path).unwrap(), sample());
        assert!(are_usable(&path));
    }

    #[test]
    fn the_file_on_disk_does_not_contain_the_password() {
        // The whole point. A regression that wrote the plaintext would still pass the
        // round-trip test above.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.bin");
        store(&path, &sample()).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let haystack = String::from_utf8_lossy(&bytes);

        assert!(
            !haystack.contains("a-long-random-password"),
            "stored in the clear"
        );
        assert!(!bytes
            .windows(sample().user.len())
            .any(|window| window == sample().user.as_bytes()));
    }

    #[test]
    fn a_corrupted_file_fails_to_decrypt_rather_than_returning_rubbish() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.bin");
        store(&path, &sample()).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        assert!(load(&path).is_err());
        assert!(!are_usable(&path));
    }

    #[test]
    fn a_file_that_is_not_dpapi_output_at_all_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.bin");
        std::fs::write(&path, b"not encrypted, just sitting here").unwrap();

        assert!(!are_usable(&path));
    }
}
