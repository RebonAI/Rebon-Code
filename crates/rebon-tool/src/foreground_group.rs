//! The process group of a foreground shell command, while a tool waits on it.
//!
//! A foreground command runs as its own process group, so that at its timeout
//! the whole tree can be handed to the background registry
//! ([`crate::shell_foreground`]). That takes it out of rebon's own group,
//! which is the group a terminal signals: Ctrl-C and a hangup used to reach
//! the command together with rebon, and a cancelled or timed-out wait used to
//! kill at least the shell's tree members that shared rebon's fate. This
//! module puts that back:
//!
//! * **Signals.** While the command runs, its group is registered here, and
//!   SIGINT and SIGHUP are forwarded to every registered group. Where rebon
//!   had no handler of its own for the signal, it then still dies of it as it
//!   always did ([`signal_hook::low_level::emulate_default_handler`]); a host
//!   that handles the signal keeps handling it.
//! * **Kill.** Dropping the [`ForegroundGroup`] — the tool call was
//!   cancelled, or the command is being killed — kills the whole group, not
//!   just the shell. [`ForegroundGroup::release`] is for the other endings: the
//!   command exited (what it left running, `nohup server &`, outlives it as
//!   before) or the registry took the group over.
//! * **A stopped group.** A command that reads the terminal (a `sudo`
//!   password, an `ssh` passphrase, `pinentry-curses`) is in a background
//!   group, so the kernel stops it with SIGTTIN/SIGTTOU instead of letting it
//!   read. [`ForegroundGroup::is_stopped`] tells the runner, which then kills
//!   it and says it needed a terminal instead of waiting or handing over a
//!   process that cannot run.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Once;

/// Groups being waited on at once, across all sessions of this process.
/// Past this many the extra ones are still killed on cancel, but signals are
/// not forwarded to them.
const SLOTS: usize = 64;
static GROUPS: [AtomicI32; SLOTS] = [const { AtomicI32::new(0) }; SLOTS];
static INSTALL: Once = Once::new();

/// A foreground command's process group, killed on drop unless released.
pub(crate) struct ForegroundGroup {
    process_group_id: i32,
    slot: Option<usize>,
    kill_on_drop: bool,
}

impl ForegroundGroup {
    /// Track the group led by `pid`, which the caller spawned as its own
    /// group leader (`process_group(0)`).
    pub(crate) fn enter(pid: u32) -> Option<Self> {
        let process_group_id = i32::try_from(pid).ok().filter(|id| *id > 0)?;
        install_forwarding();
        let slot = GROUPS.iter().position(|slot| {
            slot.compare_exchange(0, process_group_id, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        });
        Some(Self {
            process_group_id,
            slot,
            kill_on_drop: true,
        })
    }

    /// Kill every process in the group now.
    pub(crate) fn kill(&self) {
        unsafe {
            libc::kill(-self.process_group_id, libc::SIGKILL);
        }
    }

    /// Stop tracking the group without killing it.
    pub(crate) fn release(mut self) {
        self.kill_on_drop = false;
    }

    /// Whether a process of the group is stopped by job control — in
    /// practice, waiting to read or write the terminal.
    pub(crate) async fn is_stopped(&self) -> bool {
        let Ok(output) = tokio::process::Command::new("ps")
            .args(["-A", "-o", "pgid=", "-o", "stat="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .await
        else {
            return false;
        };
        group_has_a_stopped_process(
            &String::from_utf8_lossy(&output.stdout),
            self.process_group_id,
        )
    }
}

impl Drop for ForegroundGroup {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            GROUPS[slot].store(0, Ordering::Release);
        }
        if self.kill_on_drop {
            self.kill();
        }
    }
}

/// `ps -o pgid= -o stat=` output: is a process of `process_group_id`
/// stopped (`T`)?
fn group_has_a_stopped_process(ps_output: &str, process_group_id: i32) -> bool {
    ps_output.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next().and_then(|pgid| pgid.parse::<i32>().ok()) == Some(process_group_id)
            && fields.next().is_some_and(|stat| stat.starts_with('T'))
    })
}

/// Install the SIGINT and SIGHUP forwarders, once per process.
fn install_forwarding() {
    INSTALL.call_once(|| {
        for signal in [libc::SIGINT, libc::SIGHUP] {
            // Read before registering: once registered, the disposition is
            // signal-hook's own handler.
            let rebon_dies_of_it = disposition_is_default(signal);
            let registered = unsafe {
                signal_hook::low_level::register(signal, move || {
                    forward(&GROUPS, signal);
                    if rebon_dies_of_it {
                        let _ = signal_hook::low_level::emulate_default_handler(signal);
                    }
                })
            };
            if let Err(err) = registered {
                tracing::warn!(signal, error = %err, "could not forward a signal to foreground commands");
            }
        }
    });
}

/// Send `signal` to every group registered in `groups`. Async-signal-safe:
/// atomic loads and `kill` only.
fn forward(groups: &[AtomicI32], signal: i32) {
    for slot in groups {
        let process_group_id = slot.load(Ordering::Acquire);
        if process_group_id > 0 {
            unsafe {
                libc::kill(-process_group_id, signal);
            }
        }
    }
}

fn disposition_is_default(signal: i32) -> bool {
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_DFL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    fn spawn_group_leader(script: &str) -> std::process::Child {
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.process_group(0);
        command.spawn().unwrap()
    }

    fn exited_within(child: &mut std::process::Child, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn ps_lines_are_read_for_the_group_only() {
        let output = "  10 Ss\n  42 T+\n  42 S\n  77 T\n";
        assert!(group_has_a_stopped_process(output, 42));
        assert!(group_has_a_stopped_process(output, 77));
        assert!(!group_has_a_stopped_process(output, 10));
        assert!(!group_has_a_stopped_process(output, 99));
    }

    /// A group is registered for forwarding while tracked and not after.
    /// (Read from the shared table, not by sending a signal through it:
    /// other tests' foreground commands are registered there too.)
    #[test]
    fn a_tracked_group_is_registered_until_released() {
        let mut child = spawn_group_leader("sleep 30");
        let pgid = i32::try_from(child.id()).unwrap();
        let registered = || {
            GROUPS
                .iter()
                .any(|slot| slot.load(Ordering::Acquire) == pgid)
        };
        let group = ForegroundGroup::enter(child.id()).unwrap();
        assert!(registered());
        group.release();
        assert!(!registered());
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The forwarder signals every group in its table and nothing else.
    #[test]
    fn the_forwarder_signals_each_registered_group() {
        let mut target = spawn_group_leader("sleep 30");
        let mut bystander = spawn_group_leader("sleep 30");
        let table: [AtomicI32; 2] = [
            AtomicI32::new(i32::try_from(target.id()).unwrap()),
            AtomicI32::new(0),
        ];
        forward(&table, libc::SIGINT);
        assert!(exited_within(&mut target, Duration::from_secs(5)));
        assert!(!exited_within(&mut bystander, Duration::from_millis(200)));
        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    /// Dropping the group kills it whole; a released group survives.
    #[test]
    fn dropping_kills_the_group_and_releasing_does_not() {
        let mut child = spawn_group_leader("sleep 30");
        drop(ForegroundGroup::enter(child.id()).unwrap());
        assert!(exited_within(&mut child, Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn a_group_stopped_by_job_control_is_detected() {
        let mut child = spawn_group_leader("kill -STOP $$; sleep 30");
        let group = ForegroundGroup::enter(child.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stopped = false;
        while Instant::now() < deadline && !stopped {
            stopped = group.is_stopped().await;
            if !stopped {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        assert!(stopped);
        drop(group);
        let _ = child.wait();
    }
}
