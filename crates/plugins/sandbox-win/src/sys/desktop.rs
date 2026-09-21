//! Creating the confined process's window station and desktop.
//!
//! [`crate::core::desktop`] decides the name and the access masks. This is the
//! Win32 half: make the objects, grant the sandbox account onto them, keep the
//! handles, and hand `exec` the `lpDesktop` string to launch onto.
//!
//! ## The handles are the lifetime
//!
//! A station and a desktop exist for as long as a handle to them is open (or a
//! process is running on them). Nothing has to delete them, and nothing is left
//! behind if the helper is killed — which matters, because the helper being
//! killed is the normal way a command is cancelled. [`IsolatedDesktop`] holds
//! both for the whole command and closes them on drop.
//!
//! ## Why there is a window station as well
//!
//! Because a desktop on its own does not work — see [`crate::core::desktop`] for
//! the measurement. `CreateDesktopW` always creates on the *calling process's*
//! current station, so making one inside a private station means switching this
//! process onto it, creating the desktop, and switching back.
//!
//! ## `lpsa` is not how these get their security
//!
//! Both APIs take a `SECURITY_ATTRIBUTES`, and `CreateWindowStationW` **does not
//! apply it** when the name is null — measured: the station came back carrying
//! the stock station DACL with no trace of the descriptor passed in, which is why
//! granting `Everyone` full control through `lpsa` changed nothing at all.
//! Security is therefore applied afterwards, with `SetUserObjectSecurity`, for
//! both objects, by one code path. An attributes-shaped argument that quietly
//! ignores what it is given is worth one debugging session, not two.
//!
//! The grant is **added** to the DACL each object is created with, never
//! substituted for it. That stock DACL is what lets the creating user, the logon
//! session and SYSTEM use the object; replacing it with two ACEs of our own would
//! produce a station this process could not fully manage and a desktop nothing
//! could clean up.
//!
//! ## Degrading rather than refusing
//!
//! If either object cannot be made, `exec` says so on stderr and runs the command
//! on the default desktop. The account, the filters, the ACEs and the job object
//! are all still in force — this is the fourth of four mechanisms, and refusing
//! every command on a machine whose window-station policy is unusual would be a
//! worse answer than one loud line. It is never silent: a weakened sandbox that
//! says nothing is the thing this whole binary is arranged to avoid.

use crate::sys::SysResult;

/// A window station and a desktop that live as long as this value does.
pub struct IsolatedDesktop {
    /// Never read. Holding these *is* the objects' lifetime: each is
    /// destroyed when its last handle closes, which is how this cleans up
    /// after a helper that was killed rather than allowed to finish.
    #[allow(dead_code)]
    inner: imp::Handles,
    lp_desktop: String,
    name: String,
}

impl IsolatedDesktop {
    /// The `STARTUPINFO.lpDesktop` value, fully qualified.
    pub fn lp_desktop(&self) -> &str {
        &self.lp_desktop
    }

    /// The desktop's own name, for diagnostics.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Debug for IsolatedDesktop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IsolatedDesktop")
            .field("lp_desktop", &self.lp_desktop)
            .finish()
    }
}

/// Create a window station and desktop the calling user and
/// `sandbox_group_sid` can both reach, and nobody else.
pub fn create(sandbox_group_sid: &str) -> SysResult<IsolatedDesktop> {
    imp::create(sandbox_group_sid)
}

#[cfg(windows)]
mod imp {
    use super::IsolatedDesktop;
    use crate::core::desktop::{
        desktop_name, is_valid_desktop_name, is_valid_station_name, lp_desktop,
        SANDBOX_DESKTOP_ACCESS, SANDBOX_STATION_ACCESS,
    };
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{GetLastError, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, SetEntriesInAclW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        NO_MULTIPLE_TRUSTEE, TRUSTEE_IS_GROUP, TRUSTEE_IS_SID, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetUserObjectSecurity, InitializeSecurityDescriptor,
        SetSecurityDescriptorDacl, SetUserObjectSecurity, ACL, DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW,
        GetProcessWindowStation, GetUserObjectInformationW, SetProcessWindowStation, HDESK,
        HWINSTA, UOI_NAME,
    };

    /// The creator's own access to the objects it makes.
    const GENERIC_ALL: u32 = 0x1000_0000;
    /// `SECURITY_DESCRIPTOR_REVISION`
    const SD_REVISION: u32 = 1;

    pub struct Handles {
        desktop: HDESK,
        station: HWINSTA,
    }

    impl Handles {
        #[cfg(test)]
        pub fn station_handle(&self) -> HANDLE {
            self.station as HANDLE
        }

        #[cfg(test)]
        pub fn desktop_handle(&self) -> HANDLE {
            self.desktop as HANDLE
        }
    }

    impl Drop for Handles {
        fn drop(&mut self) {
            // Closing the last handle is what destroys each object. The child
            // is gone by now — `launch` waits for it — so there is nothing
            // running on either to strand. Desktop first: a station cannot go
            // while a desktop is open on it.
            if self.desktop != 0 {
                unsafe { CloseDesktop(self.desktop) };
            }
            if self.station != 0 {
                unsafe { CloseWindowStation(self.station) };
            }
        }
    }

    /// Serialises the window-station swap.
    ///
    /// `SetProcessWindowStation` is **process-global**: two threads creating
    /// isolated desktops at once leave the process on whichever station the
    /// loser restored, and every later `CreateDesktopW` — plus anything else
    /// in the process that touches USER — lands somewhere nobody chose. `exec`
    /// calls this once per process today, so the race is not reachable in
    /// production; it is reachable from the test suite, which runs its tests
    /// in threads, and that is the same bug found earlier and cheaper.
    static STATION_SWAP: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The helper's own window station, restored on drop.
    ///
    /// `CreateDesktopW` creates on whatever station the calling process is on,
    /// so making a desktop inside a private station means moving this process
    /// there first. This puts it back however the block exits — an early
    /// return that left the helper on a station it is about to destroy would
    /// be a process with no usable station at all.
    struct StationSwap {
        original: HWINSTA,
        /// Held for as long as this process is on somebody else's station.
        /// The lifetime of the guard is the lifetime of the hazard.
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl StationSwap {
        fn to(station: HWINSTA) -> SysResult<Self> {
            // A poisoned lock means a previous swap panicked mid-flight. The
            // station was still restored — that happens on unwind — so the
            // guard is taken anyway rather than turning one panic into every
            // later command failing.
            let serial = STATION_SWAP
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            let original = unsafe { GetProcessWindowStation() };
            if original == 0 {
                return Err(SysError::win32("GetProcessWindowStation", unsafe {
                    GetLastError()
                }));
            }
            if unsafe { SetProcessWindowStation(station) } == 0 {
                return Err(SysError::win32("SetProcessWindowStation", unsafe {
                    GetLastError()
                }));
            }
            Ok(Self {
                original,
                _serial: serial,
            })
        }
    }

    impl Drop for StationSwap {
        fn drop(&mut self) {
            unsafe { SetProcessWindowStation(self.original) };
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// A SID from its string form, freed on drop.
    struct OwnedSid(*mut c_void);

    impl OwnedSid {
        fn parse(text: &str) -> SysResult<Self> {
            let mut sid: *mut c_void = null_mut();
            if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid) } == 0 {
                return Err(SysError::win32("ConvertStringSidToSidW", unsafe {
                    GetLastError()
                }));
            }
            Ok(Self(sid))
        }
    }

    impl Drop for OwnedSid {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }

    /// The name Windows gave a station or desktop handle.
    fn object_name(object: HANDLE) -> SysResult<String> {
        let mut needed = 0u32;
        // The first call is expected to fail; it is how the length is asked
        // for. Only the second one's failure means anything.
        unsafe { GetUserObjectInformationW(object, UOI_NAME, null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(SysError::win32("GetUserObjectInformationW", unsafe {
                GetLastError()
            }));
        }

        let mut buffer = vec![0u16; needed as usize / 2 + 1];
        if unsafe {
            GetUserObjectInformationW(
                object,
                UOI_NAME,
                buffer.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(SysError::win32("GetUserObjectInformationW", unsafe {
                GetLastError()
            }));
        }

        let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
        Ok(String::from_utf16_lossy(&buffer[..length]))
    }

    /// Add one ACE for `sid_text` to a station's or desktop's DACL.
    ///
    /// Read-modify-write. The DACL these objects are created with is what
    /// makes them usable by the creating user, the logon session and SYSTEM;
    /// replacing it would leave a station this process cannot manage and a
    /// desktop nothing can clean up.
    fn grant(object: HANDLE, sid_text: &str, mask: u32) -> SysResult<()> {
        let mut information = DACL_SECURITY_INFORMATION;
        let mut needed = 0u32;
        unsafe { GetUserObjectSecurity(object, &mut information, null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(SysError::win32("GetUserObjectSecurity", unsafe {
                GetLastError()
            }));
        }
        let mut current = vec![0u8; needed as usize];
        if unsafe {
            GetUserObjectSecurity(
                object,
                &mut information,
                current.as_mut_ptr() as PSECURITY_DESCRIPTOR,
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(SysError::win32("GetUserObjectSecurity", unsafe {
                GetLastError()
            }));
        }

        let mut existing: *mut ACL = null_mut();
        let mut present = 0i32;
        let mut defaulted = 0i32;
        if unsafe {
            GetSecurityDescriptorDacl(
                current.as_mut_ptr() as PSECURITY_DESCRIPTOR,
                &mut present,
                &mut existing,
                &mut defaulted,
            )
        } == 0
        {
            return Err(SysError::win32("GetSecurityDescriptorDacl", unsafe {
                GetLastError()
            }));
        }

        let sid = OwnedSid::parse(sid_text)?;
        let mut entry: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        entry.grfAccessPermissions = mask;
        entry.grfAccessMode = GRANT_ACCESS;
        // No inheritance: the desktop is granted in its own right, so an ACE
        // on the station that propagated would be a second, differently
        // shaped grant on the same object.
        entry.grfInheritance = 0;
        entry.Trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_GROUP,
            ptstrName: sid.0 as *mut u16,
        };

        let mut merged: *mut ACL = null_mut();
        let result = unsafe {
            SetEntriesInAclW(
                1,
                &entry,
                if present != 0 { existing } else { null_mut() },
                &mut merged,
            )
        };
        if result != 0 {
            return Err(SysError::win32("SetEntriesInAclW", result));
        }

        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let descriptor_ptr = &mut descriptor as *mut _ as PSECURITY_DESCRIPTOR;
        let built = unsafe {
            InitializeSecurityDescriptor(descriptor_ptr, SD_REVISION) != 0
                && SetSecurityDescriptorDacl(descriptor_ptr, 1, merged, 0) != 0
                && SetUserObjectSecurity(object, &mut information, descriptor_ptr) != 0
        };
        let code = unsafe { GetLastError() };
        unsafe { LocalFree(merged as *mut c_void) };
        if !built {
            return Err(SysError::win32("SetUserObjectSecurity", code));
        }
        Ok(())
    }

    pub fn create(sandbox_group_sid: &str) -> SysResult<IsolatedDesktop> {
        let suffix = crate::sys::random::bytes(8)?
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let desktop_name = desktop_name(&suffix);
        if !is_valid_desktop_name(&desktop_name) {
            return Err(SysError::Invalid(format!(
                "{desktop_name} is not a usable desktop name"
            )));
        }

        // Resolved before anything is created: an unparseable SID has to stop
        // this here rather than leave a station behind that the sandbox
        // account was never granted on — which fails later, at process
        // creation, and looks like a different problem entirely.
        drop(OwnedSid::parse(sandbox_group_sid)?);

        // A **null name**: creating a *named* station needs create rights on
        // the session's `WindowStations` directory, which an ordinary user
        // does not have (measured — `ERROR_ACCESS_DENIED`). An anonymous one
        // is allowed, and Windows names it; that name is read back below.
        //
        // `lpsa` is null because `CreateWindowStationW` ignores it for an
        // anonymous station — see the module docs. The grant comes after.
        let station = unsafe { CreateWindowStationW(null(), 0, GENERIC_ALL, null()) };
        if station == 0 {
            return Err(SysError::win32("CreateWindowStationW", unsafe {
                GetLastError()
            }));
        }
        // Owned from here on, so every path below closes it.
        let mut handles = Handles {
            desktop: 0,
            station,
        };

        let station_name = object_name(station as HANDLE)?;
        if !is_valid_station_name(&station_name) {
            return Err(SysError::Invalid(format!(
                "Windows named the sandbox's window station {station_name:?}, which cannot be \
                 used in an lpDesktop value"
            )));
        }
        grant(station as HANDLE, sandbox_group_sid, SANDBOX_STATION_ACCESS)?;

        {
            let _swap = StationSwap::to(station)?;
            let desktop = unsafe {
                CreateDesktopW(
                    wide(&desktop_name).as_ptr(),
                    null(),
                    null(),
                    0,
                    GENERIC_ALL,
                    null(),
                )
            };
            if desktop == 0 {
                return Err(SysError::win32("CreateDesktopW", unsafe { GetLastError() }));
            }
            handles.desktop = desktop;
        }
        grant(
            handles.desktop as HANDLE,
            sandbox_group_sid,
            SANDBOX_DESKTOP_ACCESS,
        )?;

        Ok(IsolatedDesktop {
            lp_desktop: lp_desktop(&station_name, &desktop_name),
            name: desktop_name,
            inner: handles,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A well-formed SID that resolves to nobody.
        ///
        /// Deliberately not a well-known principal like `BUILTIN\Guests`:
        /// SDDL renders those as two-letter aliases on the way out, so a
        /// grant for `S-1-5-32-546` reads back as `BG` and a test looking for
        /// the SID fails on a DACL that is exactly right. An ACL stores raw
        /// SIDs and never needs them to resolve, so this is granted, stored
        /// and read back verbatim.
        const TRUSTEE: &str = "S-1-5-21-1111111111-2222222222-3333333333-1234";

        /// Serialises the tests that observe the process's window station.
        ///
        /// Separate from `STATION_SWAP`, which `create` holds internally —
        /// taking that one here would deadlock on the call. This one keeps two
        /// tests from overlapping at all, so a test reading the station back
        /// cannot catch another one mid-swap.
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn serially<T>(body: impl FnOnce() -> T) -> T {
            let _guard = SERIAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            body()
        }

        /// Read a user object's DACL back as SDDL.
        fn read_sddl(object: HANDLE) -> String {
            use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;

            let mut information = DACL_SECURITY_INFORMATION;
            let mut needed = 0u32;
            unsafe { GetUserObjectSecurity(object, &mut information, null_mut(), 0, &mut needed) };
            assert_ne!(needed, 0, "GetUserObjectSecurity asked for nothing");

            let mut buffer = vec![0u8; needed as usize];
            let ok = unsafe {
                GetUserObjectSecurity(
                    object,
                    &mut information,
                    buffer.as_mut_ptr() as PSECURITY_DESCRIPTOR,
                    needed,
                    &mut needed,
                )
            };
            assert_ne!(ok, 0, "GetUserObjectSecurity failed");

            let mut text: *mut u16 = null_mut();
            let ok = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    buffer.as_mut_ptr() as PSECURITY_DESCRIPTOR,
                    1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    null_mut(),
                )
            };
            assert_ne!(ok, 0, "ConvertSecurityDescriptor... failed");
            let mut length = 0usize;
            while unsafe { *text.add(length) } != 0 {
                length += 1;
            }
            let sddl =
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
            unsafe { LocalFree(text as *mut c_void) };
            sddl
        }

        #[test]
        fn the_grant_actually_lands_on_both_objects() {
            // The bug this test exists for: `CreateWindowStationW` accepts a
            // `SECURITY_ATTRIBUTES` and, for an anonymous station, ignores it.
            // The station came back with the stock DACL and no trace of what
            // was passed in — so the sandbox account could not attach, every
            // command that touched USER32 died at DLL initialisation with
            // `0xC0000142`, and `cmd.exe` and `hostname.exe` kept working
            // because they never touch it.
            let desktop = serially(|| create(TRUSTEE)).unwrap();
            let station = read_sddl(desktop.inner.station_handle());
            let inner = read_sddl(desktop.inner.desktop_handle());
            assert!(
                station.contains(TRUSTEE),
                "the station's DACL does not name the sandbox principal: {station}"
            );
            assert!(
                inner.contains(TRUSTEE),
                "the desktop's DACL does not name the sandbox principal: {inner}"
            );
        }

        #[test]
        fn granting_adds_to_the_stock_dacl_rather_than_replacing_it() {
            // The stock DACL is what lets this process manage the objects it
            // just made. Replacing it leaves a station the helper cannot use
            // and a desktop nothing can clean up.
            let desktop = serially(|| create(TRUSTEE)).unwrap();
            let station = read_sddl(desktop.inner.station_handle());
            assert!(station.contains(";;SY)"), "SYSTEM was dropped: {station}");
            assert!(station.contains(";;BA)"), "admins were dropped: {station}");
        }

        /// Does a process actually run on this desktop?
        ///
        /// Started as the *current* user, so it isolates the objects from the
        /// launch path. `whoami.exe` is chosen because it touches USER32: a
        /// process that cannot reach its window station dies at DLL
        /// initialisation with `0xC0000142` before `main`, and a binary that
        /// never touches USER32 (`cmd.exe`, `hostname.exe`) would pass this
        /// test with the desktop entirely unreachable.
        #[test]
        fn a_process_can_actually_run_on_the_isolated_desktop() {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                CreateProcessW, GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW,
                INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
            };

            let desktop = serially(|| create(TRUSTEE)).unwrap();
            let mut lp_desktop = wide(desktop.lp_desktop());
            let mut command = wide("whoami.exe");

            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            startup.lpDesktop = lp_desktop.as_mut_ptr();

            let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let created = unsafe {
                CreateProcessW(
                    null(),
                    command.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    CREATE_NO_WINDOW,
                    null(),
                    null(),
                    &startup,
                    &mut process,
                )
            };
            assert_ne!(created, 0, "CreateProcessW: {:?}", unsafe {
                GetLastError()
            });

            unsafe { WaitForSingleObject(process.hProcess, INFINITE) };
            let mut code = 0u32;
            unsafe { GetExitCodeProcess(process.hProcess, &mut code) };
            unsafe { CloseHandle(process.hThread) };
            unsafe { CloseHandle(process.hProcess) };

            assert_eq!(
                code, 0,
                "whoami exited {code:#x} on the isolated desktop (0xC0000142 is \
                 STATUS_DLL_INIT_FAILED — the process could not reach its window station)"
            );
        }

        #[test]
        fn a_station_and_desktop_can_be_created() {
            let desktop = serially(|| create(TRUSTEE)).unwrap();
            // `station\desktop`, where the station half is whatever Windows
            // named the anonymous one and the desktop half is ours.
            let (station, name) = desktop
                .lp_desktop()
                .rsplit_once('\\')
                .expect("a fully qualified lpDesktop");
            assert!(!station.is_empty(), "{desktop:?}");
            assert_eq!(name, desktop.name());
            assert!(desktop.name().starts_with("sandbox-win-"), "{desktop:?}");
        }

        #[test]
        fn the_station_is_not_the_one_the_helper_is_on() {
            // The whole point. A desktop on the interactive station would
            // still leave the child able to reach it.
            let (desktop, ours) = serially(|| {
                let desktop = create(TRUSTEE).unwrap();
                let ours = object_name(unsafe { GetProcessWindowStation() } as HANDLE).unwrap();
                (desktop, ours)
            });
            let (station, _) = desktop.lp_desktop().rsplit_once('\\').unwrap();
            assert_ne!(station, ours, "{desktop:?}");
        }

        #[test]
        fn the_helper_is_left_on_the_station_it_started_on() {
            // The creation moves this process onto the private station to
            // make the desktop. Not coming back would leave the helper — and
            // everything it does afterwards, including reading the child's
            // output — on a station about to be destroyed.
            //
            // Compared by name, not by handle value: a
            // `SetProcessWindowStation` round trip hands back a different
            // numeric handle to the same object.
            let (before, after) = serially(|| {
                let before = object_name(unsafe { GetProcessWindowStation() } as HANDLE).unwrap();
                let _desktop = create(TRUSTEE).unwrap();
                let after = object_name(unsafe { GetProcessWindowStation() } as HANDLE).unwrap();
                (before, after)
            });
            assert_eq!(after, before);
        }

        #[test]
        fn the_station_is_restored_even_when_the_creation_fails() {
            // `StationSwap` restores on drop rather than on the happy path,
            // which is the only version that survives an early return.
            let (before, after) = serially(|| {
                let before = object_name(unsafe { GetProcessWindowStation() } as HANDLE).unwrap();
                let _ = create("not-a-sid");
                let after = object_name(unsafe { GetProcessWindowStation() } as HANDLE).unwrap();
                (before, after)
            });
            assert_eq!(after, before);
        }

        #[test]
        fn two_desktops_do_not_collide() {
            // The suffix is random rather than the pid: two helpers can start
            // in the same millisecond, and a collision would fail the second
            // command with a name-in-use error.
            let (first, second) = serially(|| (create(TRUSTEE).unwrap(), create(TRUSTEE).unwrap()));
            assert_ne!(first.name(), second.name());
        }

        #[test]
        fn an_unresolvable_group_is_an_error_not_an_ungranted_desktop() {
            // Checked before the station is created, so a bad SID leaves
            // nothing behind — and never produces a station the sandbox
            // account was never granted on, which fails later and looks like
            // a different problem.
            assert!(create("not-a-sid").is_err());
        }

        #[test]
        fn the_group_name_is_not_accidentally_accepted_as_a_sid() {
            assert!(create(crate::core::account::SANDBOX_GROUP).is_err());
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::IsolatedDesktop;
    use crate::sys::{SysError, SysResult};

    pub struct Handles;

    pub fn create(_sandbox_group_sid: &str) -> SysResult<IsolatedDesktop> {
        Err(SysError::Unsupported("desktops"))
    }
}
