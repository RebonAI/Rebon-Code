//! Starting the confined process.
//!
//! `CreateProcessWithLogonW`, not `CreateProcessAsUserW`. The latter needs
//! `SE_ASSIGNPRIMARYTOKEN_NAME` and `SE_INCREASE_QUOTA_NAME`, which an ordinary
//! user does not have — using it would mean `exec` needed elevation, and "running
//! a command does not need administrator rights" is the sentence the whole design
//! is arranged around.
//!
//! Three things that were open questions about that choice are load-bearing here:
//!
//! 1. **Pipes reach the child.** Plain `CreatePipe` with an inheritable child end
//!    is enough; a named pipe with a permissive DACL is not needed and is not
//!    here.
//! 2. **The job can be assigned.** Doing it while the process is suspended is not
//!    required — but it is what this module does anyway, because the alternative
//!    leaves a window in which the child is running and outside the job, and
//!    anything it starts in that window escapes `KILL_ON_JOB_CLOSE`.
//! 3. **The logon type is Interactive.** Which is why `install` grants
//!    `SeInteractiveLogonRight` rather than denying it.
//!
//! ## Why the environment costs a second logon
//!
//! A process needs `SystemRoot`, `ComSpec`, `TEMP` and a dozen others to run at
//! all, and they may not be taken from the caller — the caller is a Rebon process
//! holding every API key the user has configured. So the base comes from the
//! *sandbox account's own* profile, via `LogonUser` + `CreateEnvironmentBlock`,
//! and the request is layered on top.
//!
//! Passing a null environment would avoid that logon and let
//! `CreateProcessWithLogonW` build the block itself — and would also throw away
//! every `--env` and `--unset-env`, including the proxy variables that are how a
//! `--block-network` command reaches anything at all. The extra logon is the price
//! of the environment being ours to shape.

use crate::core::env::EnvBlock;
use crate::sys::SysResult;
use std::path::Path;

/// One command to run as one account.
pub struct LaunchRequest<'a> {
    pub account: &'a str,
    pub password: &'a str,
    /// Already quoted by [`crate::core::cmdline::build_command_line`].
    /// Never assembled by concatenation: the caller's `--` argv arrives
    /// verbatim and has to survive `CommandLineToArgvW` unchanged.
    pub command_line: &'a str,
    pub working_directory: Option<&'a Path>,
    pub environment: &'a EnvBlock,
    /// `station\\desktop`, from [`crate::sys::desktop`].
    ///
    /// `None` runs on whatever desktop the caller is on. That is a weaker
    /// sandbox, not a broken one, and `exec` says so out loud when it happens
    /// rather than leaving the caller to assume the isolation it asked for.
    pub desktop: Option<&'a str>,
}

/// How the command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchOutcome {
    /// The child's own exit code, to be relayed verbatim.
    pub exit_code: i32,
}

/// The sandbox account's own environment, as the base to layer a request on.
pub fn profile_environment(account: &str, password: &str) -> SysResult<EnvBlock> {
    imp::profile_environment(account, password)
}

/// Run it, and wait.
///
/// No timeout. The caller owns timeouts — it kills this process, and
/// `KILL_ON_JOB_CLOSE` takes the whole tree down with it. A timeout here
/// would be a second, shorter, invisible one.
pub fn run(request: &LaunchRequest<'_>) -> SysResult<LaunchOutcome> {
    imp::run(request)
}

#[cfg(windows)]
mod imp {
    use super::{LaunchOutcome, LaunchRequest};
    use crate::core::env::EnvBlock;
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::{LogonUserW, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_EXITWINDOWS,
        JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
        JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessWithLogonW, GetExitCodeProcess, ResumeThread, TerminateProcess,
        WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
        INFINITE, LOGON_WITH_PROFILE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    /// `LOGON32_LOGON_INTERACTIVE`. The type it was measured
    /// `CreateProcessWithLogonW` to use, so the token this produces has the
    /// same shape as the one the command will actually run under.
    const LOGON32_LOGON_INTERACTIVE: u32 = 2;
    const LOGON32_PROVIDER_DEFAULT: u32 = 0;

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn last_error(call: &'static str) -> SysError {
        SysError::win32(call, unsafe { GetLastError() })
    }

    /// A handle closed on drop.
    struct Owned(HANDLE);

    impl Owned {
        fn take(&mut self) -> HANDLE {
            std::mem::replace(&mut self.0, 0)
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// A logon token, for reading the account's profile environment.
    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    pub fn profile_environment(account: &str, password: &str) -> SysResult<EnvBlock> {
        let mut token: HANDLE = 0;
        let ok = unsafe {
            LogonUserW(
                wide(account).as_ptr(),
                wide(".").as_ptr(),
                wide(password).as_ptr(),
                LOGON32_LOGON_INTERACTIVE,
                LOGON32_PROVIDER_DEFAULT,
                &mut token,
            )
        };
        if ok == 0 {
            return Err(logon_error("LogonUserW"));
        }
        let token = Token(token);

        let mut block: *mut c_void = null_mut();
        // `bInherit = 0`: the caller's environment must not be folded in.
        // That flag is the whole rule expressed as one argument,
        // and setting it would carry the user's API keys into the sandbox.
        if unsafe { CreateEnvironmentBlock(&mut block, token.0, 0) } == 0 {
            return Err(last_error("CreateEnvironmentBlock"));
        }

        let parsed = parse_environment_block(block);
        unsafe { DestroyEnvironmentBlock(block) };
        Ok(parsed)
    }

    /// Turn `NAME=VALUE\0NAME=VALUE\0\0` into an [`EnvBlock`].
    fn parse_environment_block(block: *const c_void) -> EnvBlock {
        let mut environment = EnvBlock::new();
        if block.is_null() {
            return environment;
        }
        let mut cursor = block as *const u16;
        loop {
            let mut length = 0usize;
            while unsafe { *cursor.add(length) } != 0 {
                length += 1;
            }
            if length == 0 {
                break;
            }
            let slice = unsafe { std::slice::from_raw_parts(cursor, length) };
            let entry = String::from_utf16_lossy(slice);
            // A block legitimately contains entries like `=C:=C:\work`, the
            // per-drive current directories. Splitting on the first `=` would
            // turn them into a variable with an empty name; the search starts
            // at one so they keep their shape and round-trip untouched.
            if let Some(split) = entry[1..].find('=').map(|index| index + 1) {
                environment.set(&entry[..split], &entry[split + 1..]);
            }
            cursor = unsafe { cursor.add(length + 1) };
        }
        environment
    }

    /// `LogonUserW` failures worth naming, because the remedy differs.
    fn logon_error(call: &'static str) -> SysError {
        let code = unsafe { GetLastError() };
        let hint = match code {
            1326 => Some(
                "the stored password does not match the account — re-run \
                 `sandbox-win.exe install`",
            ),
            1327 => Some(
                "the account is restricted (expired password, disabled, or logon hours) — \
                 re-run `sandbox-win.exe install`",
            ),
            1385 => Some(
                "the sandbox account has no interactive logon right; `install` grants it \
                 explicitly after removing the account from Users, and one of those two \
                 steps did not happen — re-run `sandbox-win.exe install`",
            ),
            _ => None,
        };
        match hint {
            Some(hint) => SysError::Invalid(format!("{call} failed ({code}): {hint}")),
            None => SysError::win32(call, code),
        }
    }

    /// One direction of the child's output.
    struct Pipe {
        parent: Owned,
        child: Owned,
    }

    impl Pipe {
        fn new() -> SysResult<Self> {
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 1,
            };
            let mut read: HANDLE = 0;
            let mut write: HANDLE = 0;
            if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
                return Err(last_error("CreatePipe"));
            }
            // Only the child's end may be inherited. A copy of the read end
            // in the child would hold the pipe open, and the read below would
            // never see end-of-file — the command would finish and the helper
            // would hang forever.
            unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) };
            Ok(Self {
                parent: Owned(read),
                child: Owned(write),
            })
        }
    }

    struct Sendable(HANDLE);
    unsafe impl Send for Sendable {}

    /// Copy one pipe to one of our own streams until it ends.
    ///
    /// A thread each, not two sequential reads: a child that fills one pipe's
    /// buffer while this process is blocked reading the other deadlocks, and
    /// the command hangs with output already written.
    ///
    /// Bytes go out exactly as they came in, with no decoding step. The
    /// child's output is whatever encoding it chose — on Windows that is
    /// frequently the OEM code page for native tools and UTF-8 for others in
    /// the same stream — and anything that decoded here would have to guess,
    /// then re-encode, and would corrupt whichever half it guessed wrong.
    fn relay(handle: HANDLE, to_stderr: bool) -> std::thread::JoinHandle<()> {
        let owned = Sendable(handle);
        std::thread::spawn(move || {
            let handle = owned;
            let mut buffer = [0u8; 8192];
            loop {
                let mut read = 0u32;
                let ok = unsafe {
                    ReadFile(
                        handle.0,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut read,
                        null_mut(),
                    )
                };
                if ok == 0 || read == 0 {
                    break;
                }
                let chunk = &buffer[..read as usize];
                // Written and flushed per chunk so a long-running command's
                // output arrives while it runs rather than at the end.
                if to_stderr {
                    let mut out = std::io::stderr();
                    let _ = out.write_all(chunk);
                    let _ = out.flush();
                } else {
                    let mut out = std::io::stdout();
                    let _ = out.write_all(chunk);
                    let _ = out.flush();
                }
            }
        })
    }

    fn open_nul() -> SysResult<Owned> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let handle = unsafe {
            CreateFileW(
                wide("NUL").as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attributes,
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error("CreateFileW(NUL)"));
        }
        Ok(Owned(handle))
    }

    /// The job the child lives in.
    ///
    /// Deliberately never closed while the child runs: `KILL_ON_JOB_CLOSE`
    /// means closing this handle kills the process whose output is still
    /// being read. It is released when the helper exits, which is exactly
    /// when the tree should go too.
    fn create_job() -> SysResult<Owned> {
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job == 0 {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = Owned(job);

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(last_error("SetInformationJobObject(limits)"));
        }

        // The UI restrictions. Not defence in depth against a
        // determined attacker — the child is a different user on its own
        // logon session already — but they close the cheap paths: reading
        // what the user copied, and asking Windows to log them out.
        let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = unsafe { std::mem::zeroed() };
        ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_EXITWINDOWS;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectBasicUIRestrictions,
                &ui as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )
        } == 0
        {
            return Err(last_error("SetInformationJobObject(ui)"));
        }

        Ok(job)
    }

    /// `NAME=VALUE\0…\0\0`, as UTF-16.
    fn environment_block(environment: &EnvBlock) -> Vec<u16> {
        let mut block: Vec<u16> = Vec::new();
        for pair in environment.to_pairs() {
            block.extend(std::ffi::OsStr::new(&pair).encode_wide());
            block.push(0);
        }
        // A block with no variables still needs its terminator, and an empty
        // `Vec` would be a null pointer — which means "inherit the caller's",
        // the one thing this must never do.
        block.push(0);
        block
    }

    pub fn run(request: &LaunchRequest<'_>) -> SysResult<LaunchOutcome> {
        let stdout = Pipe::new()?;
        let stderr = Pipe::new()?;
        let nul = open_nul()?;

        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        // stdin is NUL. A confined command that blocks on input would
        // hang with nothing to say why, and there is nobody to type at it.
        startup.hStdInput = nul.0;
        startup.hStdOutput = stdout.child.0;
        startup.hStdError = stderr.child.0;

        // Held for the whole call: `lpDesktop` is a pointer into this
        // buffer, and `CreateProcessWithLogonW` reads it after this scope
        // would have dropped a temporary.
        let mut desktop = request.desktop.map(wide);
        if let Some(desktop) = desktop.as_mut() {
            startup.lpDesktop = desktop.as_mut_ptr();
        }

        let mut command_line = wide(request.command_line);
        let mut environment = environment_block(request.environment);
        let working_directory = request
            .working_directory
            .map(|path| wide(&path.to_string_lossy()));

        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            CreateProcessWithLogonW(
                wide(request.account).as_ptr(),
                // "." is the local machine. These accounts are local by
                // construction; a domain lookup would be a network round trip
                // for a name that is not there.
                wide(".").as_ptr(),
                wide(request.password).as_ptr(),
                LOGON_WITH_PROFILE,
                null(),
                command_line.as_mut_ptr(),
                CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                environment.as_mut_ptr() as *const c_void,
                working_directory
                    .as_ref()
                    .map(|path| path.as_ptr())
                    .unwrap_or(null()),
                &startup,
                &mut process,
            )
        };
        if created == 0 {
            return Err(logon_error("CreateProcessWithLogonW"));
        }
        let mut child = Owned(process.hProcess);
        let mut thread = Owned(process.hThread);

        // Suspended, so this happens before the child has executed one
        // instruction. Assigning afterwards also works (measured
        // it) but leaves a window in which the child is running and outside
        // the job, and anything it starts in that window outlives
        // `KILL_ON_JOB_CLOSE`.
        // The child exists but is suspended. Any failure between here and
        // `ResumeThread` must *terminate* it, not merely close the handle:
        // a closed handle leaves a process nobody will ever resume — a
        // suspended orphan running as the sandbox account, holding a PID and
        // memory until the machine reboots. `TerminateProcess` before the
        // handles drop (their `Owned::drop` still closes them) is what keeps
        // "no child" from silently becoming "a stuck child".
        let job = match create_job() {
            Ok(job) => job,
            Err(error) => {
                unsafe { TerminateProcess(child.0, 1) };
                return Err(error);
            }
        };
        if unsafe { AssignProcessToJobObject(job.0, child.0) } == 0 {
            let error = last_error("AssignProcessToJobObject");
            // It never entered the job, so `KILL_ON_JOB_CLOSE` cannot reach
            // it — kill it here rather than leaking it suspended.
            unsafe { TerminateProcess(child.0, 1) };
            return Err(error);
        }

        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            let error = last_error("ResumeThread");
            drop(job);
            return Err(error);
        }
        unsafe { CloseHandle(thread.take()) };

        // This process's copies of the child's ends, dropped so the reads
        // below reach end-of-file when the child exits. Held until after the
        // spawn: closing them earlier would hand the child a closed pipe.
        drop(stdout.child);
        drop(stderr.child);
        drop(nul);

        let stdout_relay = relay(stdout.parent.0, false);
        let stderr_relay = relay(stderr.parent.0, true);

        let waited = unsafe { WaitForSingleObject(child.0, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return Err(last_error("WaitForSingleObject"));
        }

        // Joined after the wait, not before: the child is gone, so both
        // pipes are at end-of-file and neither thread can block.
        let _ = stdout_relay.join();
        let _ = stderr_relay.join();

        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(child.0, &mut code) };
        if ok == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        unsafe { CloseHandle(child.take()) };
        // The job goes last, after the child has exited, so
        // `KILL_ON_JOB_CLOSE` has nothing left to kill.
        drop(job);

        Ok(LaunchOutcome {
            exit_code: code as i32,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn an_empty_environment_still_gets_its_terminator() {
            // An empty `Vec` would be a null pointer, and null means "inherit
            // the caller's environment" — the one outcome forbidden here.
            let block = environment_block(&EnvBlock::new());
            assert_eq!(block, vec![0u16]);
        }

        #[test]
        fn the_environment_block_is_double_null_terminated() {
            let mut environment = EnvBlock::new();
            environment.set("PATH", r"C:\Windows");
            let block = environment_block(&environment);
            assert_eq!(&block[block.len() - 2..], &[0u16, 0u16]);
        }

        #[test]
        fn a_real_profile_block_round_trips() {
            // Built the way Windows builds one, then parsed back.
            let mut source: Vec<u16> = Vec::new();
            for entry in ["=C:=C:\\work", "PATH=C:\\Windows", "TEMP=C:\\Temp"] {
                source.extend(std::ffi::OsStr::new(entry).encode_wide());
                source.push(0);
            }
            source.push(0);

            let parsed = parse_environment_block(source.as_ptr() as *const c_void);

            assert_eq!(parsed.get("PATH"), Some(r"C:\Windows"));
            assert_eq!(parsed.get("TEMP"), Some(r"C:\Temp"));
            // The per-drive current directory entries keep their shape rather
            // than becoming a variable with an empty name.
            assert_eq!(parsed.get("=C:"), Some(r"C:\work"));
        }

        #[test]
        fn parsing_a_null_block_is_empty_rather_than_a_crash() {
            assert!(parse_environment_block(std::ptr::null()).is_empty());
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{LaunchOutcome, LaunchRequest};
    use crate::core::env::EnvBlock;
    use crate::sys::{SysError, SysResult};

    pub fn profile_environment(_account: &str, _password: &str) -> SysResult<EnvBlock> {
        Err(SysError::Unsupported("CreateProcessWithLogonW"))
    }

    pub fn run(_request: &LaunchRequest<'_>) -> SysResult<LaunchOutcome> {
        Err(SysError::Unsupported("CreateProcessWithLogonW"))
    }
}
