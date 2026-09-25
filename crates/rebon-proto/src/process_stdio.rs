//! The process's own stdin and stdout, taken over for a protocol connection.
//!
//! A server that speaks a protocol on stdio keeps a read parked on stdin for
//! as long as the connection is open. On Windows a pipe opened for
//! synchronous I/O serialises every operation on it, so a child handed that
//! pipe as its own stdin — which `Command::status` and `Command::spawn` do
//! unless told otherwise — while the read is parked did not get going until
//! the read completed, which is when the client happened to write its next
//! message. (A child given a null or piped stdin was never affected.)
//! `git` for the system prompt is such a child, and the turn that needed it
//! stalled with it: `rebon
//! --acp` answered `session/new` and then never finished a first prompt,
//! because an editor waits for that answer before it writes anything else.
//!
//! [`take_process_stdio`] moves the connection onto private duplicates that
//! no child inherits, and points the process's standard input and output at
//! the null device, so a child inherits nothing that belongs to the
//! connection. It does that on every platform: a child that reads the
//! connection's stdin or writes into its stdout corrupts the stream just as
//! surely as it blocks it here.

use std::io::Write;

/// The connection's two ends, detached from the process's standard handles.
pub struct ProcessStdio {
    /// What the peer writes: the process's stdin as it was.
    pub input: tokio::fs::File,
    /// What the peer reads: the process's stdout as it was.
    pub output: tokio::fs::File,
}

/// Take stdin and stdout for a protocol connection.
///
/// Call it once, before the process spawns anything, and read and write the
/// connection only through the returned ends. Afterwards `std::io::stdin()`
/// reads from and `std::io::stdout()` writes to the null device, which is
/// also what a child told to inherit them gets. Stderr is left alone: it is
/// where such a server logs.
pub fn take_process_stdio() -> std::io::Result<ProcessStdio> {
    // Whatever is still buffered was meant for the old stdout.
    std::io::stdout().flush()?;
    let (input, output) = sys::detach_standard_handles()?;
    Ok(ProcessStdio {
        input: tokio::fs::File::from_std(input),
        output: tokio::fs::File::from_std(output),
    })
}

#[cfg(unix)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsFd, AsRawFd};

    /// Duplicate fds 0 and 1 (close-on-exec, so no child inherits the
    /// copies), then put `/dev/null` in their place.
    pub(super) fn detach_standard_handles() -> std::io::Result<(File, File)> {
        let input = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        let output = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
            // SAFETY: both descriptors are open for the duration of the call;
            // `dup2` replaces `target` atomically and leaves `null` open.
            if unsafe { libc::dup2(null.as_raw_fd(), target) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok((input, output))
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::os::windows::io::{AsHandle, IntoRawHandle};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, SetStdHandle, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// Duplicate the two standard handles (not inheritable), point the
    /// process's standard handles at `NUL`, and close the originals, so no
    /// handle to the connection's pipes is left for a child to inherit.
    pub(super) fn detach_standard_handles() -> std::io::Result<(File, File)> {
        let input = File::from(std::io::stdin().as_handle().try_clone_to_owned()?);
        let output = File::from(std::io::stdout().as_handle().try_clone_to_owned()?);
        let null_input = OpenOptions::new().read(true).open("NUL")?;
        let null_output = OpenOptions::new().write(true).open("NUL")?;
        let original_input = replace_standard_handle(STD_INPUT_HANDLE, null_input)?;
        let original_output = replace_standard_handle(STD_OUTPUT_HANDLE, null_output)?;
        close_original(original_input);
        // A console hands out one handle for both; it is closed once.
        if original_output != original_input {
            close_original(original_output);
        }
        Ok((input, output))
    }

    /// Install `null` as the `which` standard handle and return the handle
    /// it replaced. The process keeps `null` for the rest of its life.
    fn replace_standard_handle(which: STD_HANDLE, null: File) -> std::io::Result<HANDLE> {
        // SAFETY: plain Win32 calls on the current process's handle table;
        // `null` is a valid handle whose ownership moves to the table.
        unsafe {
            let original = GetStdHandle(which);
            if SetStdHandle(which, null.into_raw_handle() as HANDLE) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(original)
        }
    }

    fn close_original(handle: HANDLE) {
        if handle != 0 && handle != INVALID_HANDLE_VALUE {
            // SAFETY: the handle came from `GetStdHandle` and nothing in this
            // process refers to it any more; the connection reads and writes
            // its own duplicates.
            unsafe { CloseHandle(handle) };
        }
    }
}
