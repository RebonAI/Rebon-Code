//! The confined process's own desktop.
//!
//! Window messages do not cross desktops. That single fact is the whole
//! mechanism: a process on its own desktop cannot send a message to a window on
//! the user's desktop, so the entire shatter-attack family — post a crafted
//! `WM_*` to a window belonging to a more privileged process and let it do the
//! work — has nothing to aim at.
//!
//! The sandbox account is already a different user, which stops most of it. This
//! closes the rest: two processes of different users on the *same* desktop can
//! still exchange messages, and "the sandbox cannot type into your editor" is
//! not a property to leave resting on an ACL somebody may widen.
//!
//! ## A window station too, not just a desktop
//!
//! A desktop alone does not work, and fails in a way worth writing down: naming
//! a private desktop in `lpDesktop` while leaving the child on the interactive
//! window station gets it as far as process creation and then kills it with
//! `0xC0000142` (`STATUS_DLL_INIT_FAILED`) the moment USER32 initialises. That
//! was measured — `cmd.exe` and shell builtins survive it because they never
//! touch USER32, so `echo` works and `whoami` does not, which is about the most
//! misleading shape a failure can have.
//!
//! `CreateProcessWithLogonW` grants the target account access to the *default*
//! desktop on the caller's station. Naming another one opts out of that, and the
//! station itself is still unreachable. The two ways out are to widen the
//! interactive station's DACL — a change to the user's own session that outlives
//! the command and has to be undone — or to create a private station as well and
//! put the desktop in it. This does the second. Nothing of the user's is touched,
//! and both objects die with their handles.
//!
//! ## The station has no name of our choosing
//!
//! `CreateWindowStationW` with a name needs create rights on the session's
//! `WindowStations` directory, which an ordinary user does not have — measured,
//! `ERROR_ACCESS_DENIED`. Passing a null name creates an anonymous station,
//! which is allowed, and Windows assigns it one. So the station's name is read
//! back rather than chosen, and only the desktop carries [`DESKTOP_PREFIX`].
//!
//! That is why [`is_valid_station_name`] is looser than
//! [`is_valid_desktop_name`]: one is a name this code produces and can hold to a
//! shape, the other is a name the system produces (`Service-0x0-3e7$` and the
//! like) and the only property that has to hold is the one `lpDesktop` depends
//! on.
//!
//! ## What the sandbox account is granted, and what it is not
//!
//! The desktop is created fresh, empty, and thrown away with the command, so the
//! isolation comes from it being *separate* rather than from trimming rights
//! inside it. Three rights are still withheld, because they reach back out of
//! it:
//!
//! * **`DESKTOP_SWITCHDESKTOP`** makes a desktop the active one — the sandbox
//!   could put its own empty desktop on the user's screen, which from the user's
//!   side is the machine going blank.
//! * **`DESKTOP_HOOKCONTROL`**, **`DESKTOP_JOURNALRECORD`** and
//!   **`DESKTOP_JOURNALPLAYBACK`** are the input-hooking family. Useless on an
//!   empty desktop, and the closest thing left to what this whole section exists
//!   to prevent.
//!
//! Everything else is granted: a command that draws a window nobody will ever
//! see is not a threat, and denying it produces failures that look like the
//! command is broken.

/// The prefix every window station and desktop this helper creates carries.
///
/// Named so a person looking at a desktop list — or at a leaked object after a
/// crash — can tell where it came from.
pub const DESKTOP_PREFIX: &str = "sandbox-win-";

/// Rights the sandbox account gets on its desktop.
///
/// `DESKTOP_READOBJECTS | DESKTOP_CREATEWINDOW | DESKTOP_CREATEMENU |
/// DESKTOP_ENUMERATE | DESKTOP_WRITEOBJECTS | READ_CONTROL`.
pub const SANDBOX_DESKTOP_ACCESS: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0040 | 0x0080 | 0x0002_0000;

/// `DESKTOP_SWITCHDESKTOP` — putting this desktop on the user's screen.
pub const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;
/// `DESKTOP_HOOKCONTROL` — installing a hook on this desktop.
pub const DESKTOP_HOOKCONTROL: u32 = 0x0008;
/// `DESKTOP_JOURNALRECORD`
pub const DESKTOP_JOURNALRECORD: u32 = 0x0010;
/// `DESKTOP_JOURNALPLAYBACK`
pub const DESKTOP_JOURNALPLAYBACK: u32 = 0x0020;

/// Rights the sandbox account gets on its window station.
///
/// `WINSTA_ALL_ACCESS` minus [`WINSTA_EXITWINDOWS`], plus `READ_CONTROL` and
/// **`DELETE`**.
///
/// Wide on purpose. The station is created empty, is private to one command, and
/// is destroyed with it — its clipboard and its atom table contain nothing but
/// what the command itself put there, so trimming rights *inside* it protects
/// nothing and only produces tools that fail to initialise. The isolation is
/// that it is separate.
///
/// `DELETE` is not decoration and was found the hard way. A window station is a
/// temporary object: it exists while handles to it are open, and win32k asks for
/// `DELETE` when a process attaches so the object can be torn down when the last
/// one leaves. Without it, attaching fails and the process dies at DLL
/// initialisation with `0xC0000142` — no message, no log entry, and only for
/// binaries that touch USER32, so `echo` and `hostname` work while `whoami`,
/// `git`, `node` and `powershell` do not.
///
/// Measured by bisection against a real cross-user launch: full access works,
/// full-minus-`EXITWINDOWS` works, and this mask — the trimmed one plus `DELETE`
/// alone — works. `WRITE_DAC` and `WRITE_OWNER` are *not* needed, which matters:
/// with them the sandbox could rewrite its own station's DACL and grant itself
/// back what is withheld here.
pub const SANDBOX_STATION_ACCESS: u32 = 0x0003_033F;

/// `DELETE`, as it appears in [`SANDBOX_STATION_ACCESS`].
pub const STANDARD_DELETE: u32 = 0x0001_0000;
/// `WRITE_DAC` — rewriting an object's own DACL. Never granted.
pub const STANDARD_WRITE_DAC: u32 = 0x0004_0000;
/// `WRITE_OWNER` — taking ownership. Never granted.
pub const STANDARD_WRITE_OWNER: u32 = 0x0008_0000;

/// The station grant must not let the sandbox rewrite the grant.
const _: () = assert!(SANDBOX_STATION_ACCESS & STANDARD_WRITE_DAC == 0);
const _: () = assert!(SANDBOX_STATION_ACCESS & STANDARD_WRITE_OWNER == 0);
/// …and must keep the one right that makes attaching work at all.
const _: () = assert!(SANDBOX_STATION_ACCESS & STANDARD_DELETE != 0);

/// `WINSTA_EXITWINDOWS` — asking Windows to log the user off.
///
/// The one window-station right that reaches out of a private station. Also
/// denied by the job object's UI restrictions; denied here as well because the
/// two are set in different files and either could be edited alone.
pub const WINSTA_EXITWINDOWS: u32 = 0x0040;

const _: () = assert!(SANDBOX_STATION_ACCESS & WINSTA_EXITWINDOWS == 0);

/// Whether a system-assigned window station name can be used in [`lp_desktop`].
///
/// Deliberately not [`is_valid_desktop_name`]: this name comes from Windows, not
/// from here, and rejecting it for containing a `$` would refuse a perfectly
/// good station. The one property that has to hold is the one `lpDesktop`
/// depends on — a separator in this half would silently re-point the child at
/// another station's desktop.
pub fn is_valid_station_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && !name.contains('\\') && !name.contains('/')
}

/// The security descriptor for a sandbox window station.
pub fn station_sddl(owner_sid: &str, sandbox_group_sid: &str) -> String {
    format!("D:(A;;GA;;;{owner_sid})(A;;0x{SANDBOX_STATION_ACCESS:08X};;;{sandbox_group_sid})")
}

/// The three rights that must never be in [`SANDBOX_DESKTOP_ACCESS`].
///
/// Pinned at compile time as well as in a test: this is one constant that
/// somebody widens with a `|` while chasing a command that would not draw.

const _: () = assert!(SANDBOX_DESKTOP_ACCESS & DESKTOP_SWITCHDESKTOP == 0);
const _: () = assert!(SANDBOX_DESKTOP_ACCESS & DESKTOP_HOOKCONTROL == 0);
const _: () = assert!(SANDBOX_DESKTOP_ACCESS & DESKTOP_JOURNALRECORD == 0);
const _: () = assert!(SANDBOX_DESKTOP_ACCESS & DESKTOP_JOURNALPLAYBACK == 0);

/// The desktop needs no `DELETE`, and gets none.
///
/// Asymmetric with the station on purpose, and measured rather than assumed:
/// with `DELETE` on the station the trimmed desktop mask works unchanged. A
/// right that is not needed is not granted, even for symmetry.
const _: () = assert!(SANDBOX_DESKTOP_ACCESS & STANDARD_DELETE == 0);

/// The name for a desktop, from a random suffix the caller supplies.
///
/// Random rather than the process id: two helpers starting at the same moment on
/// one machine must not collide, and a pid is reused.
pub fn desktop_name(suffix: &str) -> String {
    format!("{DESKTOP_PREFIX}{suffix}")
}

/// Whether a name can be a desktop name.
///
/// A backslash would make `lpDesktop` name a *different window station* than the
/// one the desktop was created on — the child would attach somewhere nobody
/// isolated, and it would look like it worked.
pub fn is_valid_desktop_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains('\\')
        && !name.contains('/')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

/// The `STARTUPINFO.lpDesktop` value naming `desktop` on `station`.
///
/// Fully qualified on purpose. The bare form means "this desktop on whatever
/// window station the child ends up on", and the child is created by another
/// service on our behalf — leaving that to be resolved elsewhere is how a
/// process ends up somewhere other than where it was put.
pub fn lp_desktop(station: &str, desktop: &str) -> String {
    format!("{station}\\{desktop}")
}

/// The security descriptor for a sandbox desktop.
///
/// `owner_sid` is the calling user, who has to keep the handle open for as long
/// as the command runs — the desktop is destroyed when the last handle closes,
/// and that is deliberately how it gets cleaned up.
pub fn desktop_sddl(owner_sid: &str, sandbox_group_sid: &str) -> String {
    format!("D:(A;;GA;;;{owner_sid})(A;;0x{SANDBOX_DESKTOP_ACCESS:08X};;;{sandbox_group_sid})")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "S-1-5-21-1-2-3-1000";
    const GROUP: &str = "S-1-5-21-1-2-3-1010";

    #[test]
    fn the_sandbox_cannot_put_its_desktop_on_the_users_screen() {
        // The one right whose absence a user would notice immediately if it were
        // present: switching desktops blanks the machine.
        assert_eq!(SANDBOX_DESKTOP_ACCESS & DESKTOP_SWITCHDESKTOP, 0);
    }

    #[test]
    fn the_sandbox_cannot_hook_input_on_its_desktop() {
        for right in [
            DESKTOP_HOOKCONTROL,
            DESKTOP_JOURNALRECORD,
            DESKTOP_JOURNALPLAYBACK,
        ] {
            assert_eq!(SANDBOX_DESKTOP_ACCESS & right, 0, "{right:#x} was granted");
        }
    }

    #[test]
    fn the_sandbox_can_do_the_ordinary_things_a_process_does() {
        // Withholding these produces commands that fail in ways that read as the command
        // being broken, for no security gain — there is nothing on this desktop to
        // protect.
        for (name, right) in [
            ("READOBJECTS", 0x0001),
            ("CREATEWINDOW", 0x0002),
            ("CREATEMENU", 0x0004),
            ("ENUMERATE", 0x0040),
            ("WRITEOBJECTS", 0x0080),
        ] {
            assert_ne!(SANDBOX_DESKTOP_ACCESS & right, 0, "{name} was withheld");
        }
    }

    #[test]
    fn the_sandbox_cannot_log_the_user_off_from_its_window_station() {
        // The one right on a private station that reaches outside it. The job object
        // denies it too — this is the second of the two, because they live in different
        // files and either could be edited alone.
        assert_eq!(SANDBOX_STATION_ACCESS & WINSTA_EXITWINDOWS, 0);
    }

    #[test]
    fn the_station_grant_includes_delete() {
        // The bit whose absence produced `0xC0000142` at DLL initialisation for every
        // command that touches USER32 — and only for those, so `echo` worked and `git`
        // did not.
        assert_ne!(SANDBOX_STATION_ACCESS & STANDARD_DELETE, 0);
    }

    #[test]
    fn the_sandbox_cannot_rewrite_its_own_station_grant() {
        // With `WRITE_DAC` it could add back `EXITWINDOWS`, and with `WRITE_OWNER` it
        // could take the object and do as it liked. Measured as unnecessary, so
        // withheld.
        assert_eq!(SANDBOX_STATION_ACCESS & STANDARD_WRITE_DAC, 0);
        assert_eq!(SANDBOX_STATION_ACCESS & STANDARD_WRITE_OWNER, 0);
    }

    #[test]
    fn the_desktop_grant_is_not_widened_to_match_the_station() {
        assert_eq!(SANDBOX_DESKTOP_ACCESS & STANDARD_DELETE, 0);
    }

    #[test]
    fn the_station_grant_is_otherwise_wide() {
        // Deliberately. A private, empty, single-command station has nothing in it to
        // protect, and a trimmed right here shows up as a tool failing to initialise
        // rather than as a sandbox working harder.
        for (name, right) in [
            ("ENUMDESKTOPS", 0x0001),
            ("READATTRIBUTES", 0x0002),
            ("ACCESSCLIPBOARD", 0x0004),
            ("CREATEDESKTOP", 0x0008),
            ("WRITEATTRIBUTES", 0x0010),
            ("ACCESSGLOBALATOMS", 0x0020),
            ("ENUMERATE", 0x0100),
            ("READSCREEN", 0x0200),
        ] {
            assert_ne!(SANDBOX_STATION_ACCESS & right, 0, "{name} was withheld");
        }
    }

    #[test]
    fn a_system_assigned_station_name_is_accepted() {
        // The shape Windows actually hands back for an anonymous station. The desktop
        // rule would reject the `$` and there would be no station to launch onto.
        assert!(is_valid_station_name("Service-0x0-3e7$"));
        assert!(!is_valid_desktop_name("Service-0x0-3e7$"));
    }

    #[test]
    fn a_station_name_with_a_separator_is_still_refused() {
        // The one property `lpDesktop` depends on.
        assert!(!is_valid_station_name(r"a\b"));
        assert!(!is_valid_station_name(""));
    }

    #[test]
    fn the_station_descriptor_names_both_principals() {
        let sddl = station_sddl(OWNER, GROUP);
        assert!(sddl.contains(OWNER), "{sddl}");
        assert!(sddl.contains(GROUP), "{sddl}");
        assert!(sddl.starts_with("D:"), "{sddl}");
    }

    #[test]
    fn the_descriptor_names_both_principals() {
        let sddl = desktop_sddl(OWNER, GROUP);
        assert!(sddl.contains(OWNER), "{sddl}");
        assert!(sddl.contains(GROUP), "{sddl}");
        assert!(sddl.starts_with("D:"), "{sddl}");
    }

    #[test]
    fn the_group_gets_the_trimmed_mask_and_not_generic_all() {
        // Written as a hex mask rather than `GA` precisely so the three withheld rights
        // stay withheld. A `GA` here would grant all of them and the DACL would read as
        // though it were tighter than it is.
        let sddl = desktop_sddl(OWNER, GROUP);
        let group_ace = sddl
            .split("(A;;")
            .find(|ace| ace.contains(GROUP))
            .expect("the group's ACE");
        assert!(group_ace.starts_with("0x"), "{sddl}");
        assert!(!group_ace.starts_with("GA"), "{sddl}");
    }

    #[test]
    fn a_name_with_a_separator_is_refused() {
        // `lpDesktop` is `station\desktop`. A backslash in the desktop half would
        // silently re-point the child at another window station.
        assert!(!is_valid_desktop_name(r"other\default"));
        assert!(!is_valid_desktop_name("other/default"));
        assert!(!is_valid_desktop_name(""));
    }

    #[test]
    fn a_generated_name_is_always_valid() {
        for suffix in ["0", "deadbeef", "0123456789abcdef"] {
            let name = desktop_name(suffix);
            assert!(is_valid_desktop_name(&name), "{name}");
            assert!(name.starts_with(DESKTOP_PREFIX), "{name}");
        }
    }

    #[test]
    fn the_lp_desktop_value_is_fully_qualified() {
        assert_eq!(
            lp_desktop("WinSta0", &desktop_name("abc123")),
            r"WinSta0\sandbox-win-abc123"
        );
    }
}
