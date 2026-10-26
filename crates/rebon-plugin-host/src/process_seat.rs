//! The `process` seat: programs a composition starts, started by rebon.
//!
//! A plugin on Verve cannot start a program — Verve has no `child_process`,
//! and DeepSeek Harness's own subprocess backend (`dsh-subprocess-local`) is
//! native addons besides. So the realm answers dsh's `subprocess` seam itself
//! (`compose-runtime/src/subprocess-runtime.mjs`), and this is the half that
//! runs the program: `spawn` starts it and answers with an id, `wait` hands
//! back what it wrote since the last `wait` (holding until there is
//! something, it ends, or the wait's own bound), and `kill` ends it.
//!
//! Two gates stand in front of it besides the plugin declaring the seat:
//!
//! * **the shared host only.** A contained plugin is one rebon confines to
//!   its own files and no processes; a program rebon started for it would be
//!   outside that container. Its calls are refused by name.
//! * **the model's tool call is the user's.** What starts a program is a
//!   composition tool (`pwsh`, `bash`), and a composition tool that is not
//!   read-only asks before it runs (`kernel_compose_tools`).
//!
//! plane 关闭时显式终止并等待它启动的直接子进程，而不是等待共享 Tokio runtime
//! 销毁。`kill_on_drop` 仅作为宿主异常退出时的最后一道保护。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_plugin_supervisor::ToolRefusal;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

/// The seat's name, as a manifest declares it.
pub const PROCESS_SEAT: &str = "process";

/// The most one stream keeps before dropping its head: what a consumer has
/// not read by then it reads as lost, as dsh's bounded collection does.
const MAX_BUFFERED: usize = 32 * 1024 * 1024;

/// The longest one `wait` holds before answering with nothing new.
const MAX_WAIT: Duration = Duration::from_secs(30);

fn refused(code: &str, message: impl Into<String>) -> ToolRefusal {
    ToolRefusal::new(code, message)
}

#[derive(Default)]
struct Stream {
    /// Bytes not yet handed out.
    pending: Vec<u8>,
    /// Bytes dropped because nobody read them in time.
    dropped: u64,
    closed: bool,
}

#[derive(Default)]
struct Output {
    stdout: Stream,
    stderr: Stream,
    exit: Option<Value>,
}

struct Running {
    owner: String,
    output: Arc<Mutex<Output>>,
    changed: Arc<Notify>,
    /// Asks the task that owns the child to end it.
    kill: Option<tokio::sync::oneshot::Sender<()>>,
    /// plane 关闭时等待直接子进程被回收，不把 JoinHandle 丢弃成后台孤儿。
    exited: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct ProcessTable {
    closed: bool,
    running: BTreeMap<u64, Running>,
}

/// The programs this plane started, by id.
#[derive(Default)]
pub struct ProcessSeat {
    next: AtomicU64,
    table: Mutex<ProcessTable>,
}

impl ProcessSeat {
    /// Answers one `process.*` call made by `owner`.
    pub async fn call(
        &self,
        owner: &str,
        method: &str,
        params: &Value,
    ) -> Result<Value, ToolRefusal> {
        match method {
            "spawn" => self.spawn(owner, params),
            "wait" => self.wait(owner, params).await,
            "kill" => self.kill(owner, params).await,
            other => Err(refused(
                "[UNKNOWN_METHOD]",
                format!("the process seat has no method {other:?}"),
            )),
        }
    }

    fn spawn(&self, owner: &str, params: &Value) -> Result<Value, ToolRefusal> {
        // 与 shutdown 共用锁，避免关闭取走进程表之后又启动一个未被回收的进程。
        let mut table = self.table.lock().expect("process table poisoned");
        if table.closed {
            return Err(refused("[CLOSED]", "the process seat has shut down"));
        }
        let argv: Option<Vec<&str>> = params
            .get("argv")
            .and_then(Value::as_array)
            .and_then(|list| list.iter().map(Value::as_str).collect());
        let Some((program, args)) = argv.as_deref().and_then(<[&str]>::split_first) else {
            return Err(refused(
                "[WRONG_SHAPE]",
                "process.spawn needs a non-empty argv of strings",
            ));
        };
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        // dsh's `scrubbedParentEnv`: the child starts from rebon's own
        // environment minus anything credential-shaped and minus dsh's
        // namespace; what a plugin deliberately forwards comes after.
        command.env_clear();
        command.envs(std::env::vars_os().filter(|(name, _)| !is_scrubbed(&name.to_string_lossy())));
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            command.current_dir(cwd);
        }
        if let Some(env) = params.get("env").and_then(Value::as_object) {
            for (name, value) in env {
                match value.as_str() {
                    Some(value) => {
                        command.env(name, value);
                    }
                    // dsh's tombstone: `undefined` removes an ambient entry,
                    // which JSON carries as null.
                    None => {
                        command.env_remove(name);
                    }
                }
            }
        }
        let stdin = params
            .get("stdin")
            .and_then(Value::as_str)
            .map(str::to_owned);
        command.stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        });
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command.kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| refused("[SPAWN_FAILED]", format!("{program}: {error}")))?;
        let output = Arc::new(Mutex::new(Output::default()));
        let changed = Arc::new(Notify::new());
        if let (Some(mut pipe), Some(text)) = (child.stdin.take(), stdin) {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let _ = pipe.write_all(text.as_bytes()).await;
            });
        }
        let pump = |reader: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
                    pick: fn(&mut Output) -> &mut Stream| {
            let output = Arc::clone(&output);
            let changed = Arc::clone(&changed);
            tokio::spawn(async move {
                let Some(mut reader) = reader else {
                    pick(&mut output.lock().expect("process output poisoned")).closed = true;
                    changed.notify_waiters();
                    return;
                };
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    let read = reader.read(&mut buffer).await.unwrap_or(0);
                    {
                        let mut output = output.lock().expect("process output poisoned");
                        let stream = pick(&mut output);
                        if read == 0 {
                            stream.closed = true;
                        } else {
                            stream.pending.extend_from_slice(&buffer[..read]);
                            if stream.pending.len() > MAX_BUFFERED {
                                let excess = stream.pending.len() - MAX_BUFFERED;
                                stream.pending.drain(..excess);
                                stream.dropped += excess as u64;
                            }
                        }
                    }
                    changed.notify_waiters();
                    if read == 0 {
                        return;
                    }
                }
            });
        };
        pump(
            child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            |output| &mut output.stdout,
        );
        pump(
            child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            |output| &mut output.stderr,
        );
        let (kill, killed) = tokio::sync::oneshot::channel::<()>();
        let exited = {
            let output = Arc::clone(&output);
            let changed = Arc::clone(&changed);
            // The child's one owner: it waits for the exit, and ends the
            // child when `kill` asks.
            tokio::spawn(async move {
                let status = tokio::select! {
                    status = child.wait() => status.ok(),
                    Ok(()) = killed => {
                        let _ = child.start_kill();
                        child.wait().await.ok()
                    }
                };
                output.lock().expect("process output poisoned").exit = Some(exit_of(status));
                changed.notify_waiters();
            })
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        table.running.insert(
            id,
            Running {
                owner: owner.to_owned(),
                output,
                changed,
                kill: Some(kill),
                exited,
            },
        );
        Ok(json!({ "id": id }))
    }

    /// The running program `params.id` names, if it is the caller's.
    fn find(
        &self,
        owner: &str,
        params: &Value,
    ) -> Result<(u64, Arc<Mutex<Output>>, Arc<Notify>), ToolRefusal> {
        let id = params.get("id").and_then(Value::as_u64).ok_or_else(|| {
            refused(
                "[WRONG_SHAPE]",
                "a process call names the process by its id",
            )
        })?;
        let table = self.table.lock().expect("process table poisoned");
        let process = table
            .running
            .get(&id)
            .filter(|process| process.owner == owner)
            .ok_or_else(|| {
                refused(
                    "[UNKNOWN_PROCESS]",
                    format!("no process {id} was started by this plugin"),
                )
            })?;
        Ok((
            id,
            Arc::clone(&process.output),
            Arc::clone(&process.changed),
        ))
    }

    async fn wait(&self, owner: &str, params: &Value) -> Result<Value, ToolRefusal> {
        let (id, output, changed) = self.find(owner, params)?;
        let bound = params
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(MAX_WAIT)
            .min(MAX_WAIT);
        let deadline = tokio::time::Instant::now() + bound;
        loop {
            let notified = changed.notified();
            {
                let mut output = output.lock().expect("process output poisoned");
                let finished =
                    output.exit.is_some() && output.stdout.closed && output.stderr.closed;
                let has_output =
                    !output.stdout.pending.is_empty() || !output.stderr.pending.is_empty();
                if has_output || finished || tokio::time::Instant::now() >= deadline {
                    let take = |stream: &mut Stream| {
                        let bytes = std::mem::take(&mut stream.pending);
                        let dropped = std::mem::take(&mut stream.dropped);
                        json!({ "text": String::from_utf8_lossy(&bytes), "bytes": bytes.len(), "dropped": dropped })
                    };
                    let answer = json!({
                        "stdout": take(&mut output.stdout),
                        "stderr": take(&mut output.stderr),
                        "exit": if finished { output.exit.clone().unwrap_or(Value::Null) } else { Value::Null },
                    });
                    drop(output);
                    if finished {
                        self.table
                            .lock()
                            .expect("process table poisoned")
                            .running
                            .remove(&id);
                    }
                    return Ok(answer);
                }
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    async fn kill(&self, owner: &str, params: &Value) -> Result<Value, ToolRefusal> {
        let (id, _, _) = self.find(owner, params)?;
        let kill = self
            .table
            .lock()
            .expect("process table poisoned")
            .running
            .get_mut(&id)
            .and_then(|process| process.kill.take());
        // Asked once: a second kill, or one after the exit, finds nothing to ask.
        let asked = kill.is_some_and(|kill| kill.send(()).is_ok());
        Ok(json!({ "killed": asked }))
    }

    /// 先禁止新启动并同时通知所有子进程，再逐个等待回收。
    pub async fn shutdown(&self) {
        let running = {
            let mut table = self.table.lock().expect("process table poisoned");
            table.closed = true;
            std::mem::take(&mut table.running)
        };
        let mut exits = Vec::new();
        for (_, mut process) in running {
            if let Some(kill) = process.kill.take() {
                // 已退出的子进程会关闭接收端，无须再次终止。
                let _ = kill.send(());
            }
            exits.push(process.exited);
        }
        for exit in exits {
            if let Err(error) = exit.await {
                tracing::warn!(%error, "回收 composition 子进程的任务异常结束");
            }
        }
    }
}

/// Whether a parent variable stays out of a child's environment: the
/// credential-shaped names and `DSH_*`, case-insensitively, as dsh's
/// `scrubbedParentEnv` keeps them out.
fn is_scrubbed(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["KEY", "PASSWORD", "SECRET", "TOKEN"]
        .iter()
        .any(|marker| name.contains(marker))
        || name.starts_with("DSH_")
}

/// dsh's `SubprocessOutcome`: an exit code, or the signal that ended it.
fn exit_of(status: Option<std::process::ExitStatus>) -> Value {
    let Some(status) = status else {
        return json!({ "exitCode": null, "signal": "SIGKILL" });
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            let name = match signal {
                9 => "SIGKILL",
                15 => "SIGTERM",
                2 => "SIGINT",
                1 => "SIGHUP",
                _ => "SIGTERM",
            };
            return json!({ "exitCode": null, "signal": name });
        }
    }
    json!({ "exitCode": status.code(), "signal": null })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo(text: &str) -> Value {
        if cfg!(windows) {
            json!(["cmd", "/c", format!("echo {text}& echo oops 1>&2")])
        } else {
            json!(["sh", "-c", format!("echo {text}; echo oops 1>&2")])
        }
    }

    async fn drain(seat: &ProcessSeat, owner: &str, id: u64) -> (String, String, Value) {
        let (mut out, mut err) = (String::new(), String::new());
        loop {
            let answer = seat
                .call(owner, "wait", &json!({ "id": id, "timeoutMs": 5000 }))
                .await
                .unwrap();
            out.push_str(answer["stdout"]["text"].as_str().unwrap());
            err.push_str(answer["stderr"]["text"].as_str().unwrap());
            if !answer["exit"].is_null() {
                return (out, err, answer["exit"].clone());
            }
        }
    }

    #[test]
    fn credentials_and_dsh_facts_stay_out_of_a_child() {
        for name in [
            "DEEPSEEK_API_KEY",
            "github_token",
            "Db_Password",
            "MY_SECRET",
            "DSH_HOME",
            "dsh_x",
        ] {
            assert!(is_scrubbed(name), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "HTTPS_PROXY", "SystemRoot"] {
            assert!(!is_scrubbed(name), "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_runs_and_its_output_and_exit_come_back() {
        let seat = ProcessSeat::default();
        let started = seat
            .call(
                "shell",
                "spawn",
                &json!({ "argv": echo("hello"), "env": { "REBON_TEST_UNSET": null } }),
            )
            .await
            .unwrap();
        let id = started["id"].as_u64().unwrap();
        let (out, err, exit) = drain(&seat, "shell", id).await;
        assert_eq!(out.trim(), "hello");
        assert_eq!(err.trim(), "oops");
        assert_eq!(exit, json!({ "exitCode": 0, "signal": null }));
        let gone = seat
            .call("shell", "wait", &json!({ "id": id }))
            .await
            .unwrap_err();
        assert_eq!(
            gone.code, "[UNKNOWN_PROCESS]",
            "a finished process is forgotten"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stdin_is_written_and_a_program_is_the_starters_alone() {
        let seat = ProcessSeat::default();
        let argv = if cfg!(windows) {
            json!(["cmd", "/c", "more"])
        } else {
            json!(["cat"])
        };
        let started = seat
            .call(
                "shell",
                "spawn",
                &json!({ "argv": argv, "stdin": "piped in\n" }),
            )
            .await
            .unwrap();
        let id = started["id"].as_u64().unwrap();
        let other = seat
            .call("other", "kill", &json!({ "id": id }))
            .await
            .unwrap_err();
        assert_eq!(other.code, "[UNKNOWN_PROCESS]");
        let (out, _, exit) = drain(&seat, "shell", id).await;
        assert!(out.contains("piped in"), "{out:?}");
        assert_eq!(exit["exitCode"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutting_down_reaps_running_children_and_refuses_new_spawns() {
        let seat = ProcessSeat::default();
        let argv = if cfg!(windows) {
            json!(["ping", "-n", "30", "127.0.0.1"])
        } else {
            json!(["sleep", "30"])
        };
        let started = seat
            .call("shell", "spawn", &json!({ "argv": argv }))
            .await
            .unwrap();
        let id = started["id"].as_u64().unwrap();
        let (_, output, _) = seat.find("shell", &json!({ "id": id })).unwrap();
        tokio::time::timeout(Duration::from_secs(5), seat.shutdown())
            .await
            .expect("直接子进程被回收");
        assert!(output.lock().unwrap().exit.is_some());
        assert!(seat.table.lock().unwrap().running.is_empty());
        let refused = seat
            .call("shell", "spawn", &json!({ "argv": echo("late") }))
            .await
            .unwrap_err();
        assert_eq!(refused.code, "[CLOSED]");
        seat.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_killed_program_ends_and_a_bad_spawn_is_refused() {
        let seat = ProcessSeat::default();
        let argv = if cfg!(windows) {
            json!(["cmd", "/c", "ping -n 30 127.0.0.1 > nul"])
        } else {
            json!(["sleep", "30"])
        };
        let started = seat
            .call("shell", "spawn", &json!({ "argv": argv }))
            .await
            .unwrap();
        let id = started["id"].as_u64().unwrap();
        let killed = seat
            .call("shell", "kill", &json!({ "id": id }))
            .await
            .unwrap();
        assert_eq!(killed["killed"], true);
        let (_, _, exit) = drain(&seat, "shell", id).await;
        assert_ne!(exit["exitCode"], 0, "{exit}");

        let missing = seat
            .call(
                "shell",
                "spawn",
                &json!({ "argv": ["rebon-no-such-program-exists"] }),
            )
            .await
            .unwrap_err();
        assert_eq!(missing.code, "[SPAWN_FAILED]");
        let empty = seat
            .call("shell", "spawn", &json!({ "argv": [] }))
            .await
            .unwrap_err();
        assert_eq!(empty.code, "[WRONG_SHAPE]");
    }
}
