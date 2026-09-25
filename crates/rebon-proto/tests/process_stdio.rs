//! A server that took its stdio can start children while its read is parked,
//! and the children get nothing of the connection.
//!
//! One process cannot test this about itself: the property is about the
//! handles a process was started with. So the test starts this same binary
//! again as the server, on pipes it owns, and plays the client. The server
//! takes its stdio, parks a read on the connection, and then starts a child
//! that inherits its standard handles and reads stdin to the end, then writes
//! a line. With the connection's pipes still in the standard slots that child
//! either blocks (Windows, until the client writes) or eats the client's
//! bytes and writes into the protocol stream (everywhere); with them taken, it
//! reads the null device, writes to it, and is done. Every wait has a
//! deadline, so a regression fails instead of hanging.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Set in the environment of the copy of this binary that plays the server.
const SERVER_ROLE: &str = "REBON_PROTO_PROCESS_STDIO_SERVER";
const TEST_NAME: &str = "a_child_started_during_a_parked_read_gets_nothing_of_the_connection";
const DEADLINE: Duration = Duration::from_secs(30);

#[test]
fn a_child_started_during_a_parked_read_gets_nothing_of_the_connection() {
    if std::env::var_os(SERVER_ROLE).is_some() {
        serve();
        return;
    }

    let mut server = Command::new(std::env::current_exe().expect("this test binary"))
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(SERVER_ROLE, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the server copy");
    let mut to_server = server.stdin.take().expect("server stdin");
    let lines = {
        let (sender, receiver) = mpsc::channel();
        let stdout = server.stdout.take().expect("server stdout");
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        receiver
    };
    // The copy's test harness writes on the same stdout before the server
    // takes it, `test <name> ... ` without a newline among it: keep only what
    // follows one of our markers.
    let next = |what: &str| loop {
        let line = lines
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("the server did not say `{what}` within {DEADLINE:?}"));
        if let Some(at) = line.find("server:").or_else(|| line.find("leak")) {
            return line[at..].to_string();
        }
    };

    // Written only after the child finished: until then the client writes
    // nothing, which is exactly what an editor waiting for an answer does.
    assert_eq!(next("spawned"), "server:spawned");
    writeln!(to_server, "ping").expect("write to the server");
    to_server.flush().expect("flush to the server");
    assert_eq!(next("ping"), "server:got ping");
    drop(to_server);

    let started = Instant::now();
    loop {
        if let Some(status) = server.try_wait().expect("poll the server") {
            assert!(status.success(), "the server copy failed: {status}");
            break;
        }
        assert!(started.elapsed() < DEADLINE, "the server copy did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The server's side: take stdio, park a read, start a child that inherits
/// the standard handles, report, then answer what the client sends.
fn serve() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let stdio = rebon_proto::process_stdio::take_process_stdio().expect("take stdio");
        let mut output = stdio.output;
        let mut input = tokio::io::BufReader::new(stdio.input).lines();
        let read = tokio::spawn(async move { input.next_line().await });
        // Let the read reach the pipe before anything is started.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut child = if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", "more & echo leak"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "cat; echo leak"]);
            command
        }
        .spawn()
        .expect("start a child that inherits the standard handles");
        let started = Instant::now();
        while child.try_wait().expect("poll the child").is_none() {
            if started.elapsed() > DEADLINE {
                let _ = child.kill();
                panic!("a child that inherits stdin did not finish while the read was parked");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        output
            .write_all(b"server:spawned\n")
            .await
            .expect("write to the client");
        output.flush().await.expect("flush to the client");
        let line = tokio::time::timeout(DEADLINE, read)
            .await
            .expect("the client's line arrives")
            .expect("the read task")
            .expect("read from the client")
            .expect("a line, not EOF");
        output
            .write_all(format!("server:got {line}\n").as_bytes())
            .await
            .expect("write to the client");
        output.flush().await.expect("flush to the client");
    });
}
