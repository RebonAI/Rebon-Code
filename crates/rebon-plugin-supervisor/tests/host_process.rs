//! The supervisor against real child processes: `rebon-verve` hosts.
//!
//! They need the runner built (`cargo build -p rebon-verve-host --bin
//! rebon-verve`) or named by `REBON_VERVE`; without it they fail and say so,
//! rather than skip.
//!
//! Most cases drive purpose-built scripts rather than the real host, because
//! what is being tested is how the supervisor behaves when a host misbehaves,
//! and the real host is written not to.

use std::{env, path::PathBuf, time::Duration};

use rebon_plugin_supervisor::{
    HostCallError, HostConfig, HostEngine, PluginHostSupervisor, SupervisorError,
};

/// The runner a process host starts: `REBON_VERVE`, or the `rebon-verve` this
/// target directory built (`cargo build -p rebon-verve-host --bin rebon-verve`).
fn runner() -> PathBuf {
    if let Some(path) = env::var_os("REBON_VERVE") {
        return PathBuf::from(path);
    }
    let exe = env::current_exe().expect("the test knows its own path");
    let name = if cfg!(windows) {
        "rebon-verve.exe"
    } else {
        "rebon-verve"
    };
    let built = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("a test binary lives in target/<profile>/deps")
        .join(name);
    assert!(
        built.is_file(),
        "process hosts need the runner: build it with \
         `cargo build -p rebon-verve-host --bin rebon-verve`, or set REBON_VERVE"
    );
    built
}

fn real_host() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/plugin-host/src/cli.mjs")
}

/// Writes a throwaway host that behaves however a test needs.
fn fake_host(dir: &std::path::Path, body: &str) -> PathBuf {
    let script = dir.join("fake-host.mjs");
    std::fs::write(&script, body).unwrap();
    script
}

/// A process host with nothing confining it: what these cases test is the
/// supervisor, not the permission model.
fn config(runner: &std::path::Path, script: PathBuf) -> HostConfig {
    let mut config = HostConfig::new(runner, script)
        .with_engine(HostEngine::Process)
        .with_startup_timeout(Duration::from_secs(20))
        .with_working_directory(env::temp_dir());
    config.runner_args = vec!["--allow-all".into()];
    config
}

#[tokio::test]
async fn the_supervisor_drives_the_real_host_through_its_lifecycle() {
    let runner = runner();
    let supervisor = PluginHostSupervisor::start(config(&runner, real_host()))
        .await
        .expect("host starts and initializes");
    assert!(supervisor.is_alive().await);
    assert_eq!(supervisor.host_epoch().await, 1);

    let generation = supervisor
        .open_scope("plugin.a", "session-1", "C:/workspace")
        .await
        .expect("scope opens");
    assert_eq!(generation, 0, "the first incarnation of a scope");

    let revoked = supervisor
        .close_scope("plugin.a", "session-1")
        .await
        .expect("scope closes");
    assert!(revoked.is_empty(), "nothing was subscribed");

    supervisor.shutdown().await.expect("host shuts down");
    // A shut-down host is gone, but it did not crash, and the recorded reason
    // has to say which.
    assert!(!supervisor.is_alive().await);
    assert!(supervisor.was_shut_down().await);
    assert_eq!(
        supervisor.failure().await.unwrap().reason,
        rebon_plugin_supervisor::SHUTDOWN_REASON
    );
}

/// A method the host does not implement is the host answering, not the host
/// breaking: the call fails and everything else keeps working.
#[tokio::test]
async fn an_unsupported_method_is_a_rejection_not_a_host_failure() {
    let runner = runner();
    let supervisor = PluginHostSupervisor::start(config(&runner, real_host()))
        .await
        .unwrap();

    let identity = rebon_plugin_protocol::CallIdentity::platform_control(1, "probe").unwrap();
    let error = supervisor
        .request(
            identity,
            // `seat/call` travels Node → rebon, so a host having no handler
            // for it is permanent rather than a gap waiting to be filled —
            // which keeps this test from chasing each method as it lands.
            "seat/call",
            rebon_plugin_protocol::Payload::null(),
        )
        .await
        .unwrap_err();
    match error {
        HostCallError::Rejected { payload, .. } => {
            assert_eq!(payload.to_value().unwrap()["code"], "unknown_method");
        }
        other => panic!("expected a rejection, got {other}"),
    }

    assert!(supervisor.is_alive().await, "the host is still usable");
    supervisor
        .open_scope("plugin.a", "session-1", "C:/workspace")
        .await
        .expect("and still answers");
    supervisor.shutdown().await.unwrap();
}

/// The rule the state machine encodes, proven against a process that really dies.
#[tokio::test]
async fn a_host_that_exits_immediately_fails_the_handshake_with_its_stderr() {
    let runner = runner();
    let dir = tempfile::tempdir().unwrap();
    let script = fake_host(
        dir.path(),
        "process.stderr.write('boom: the host could not start\\n');\nprocess.exit(9);\n",
    );

    let error = PluginHostSupervisor::start(config(&runner, script))
        .await
        .expect_err("a host that exits cannot initialize");
    let rendered = error.to_string();
    assert!(
        rendered.contains("boom: the host could not start"),
        "the failure must carry the host's own diagnosis: {rendered}"
    );
}

/// Stdout is the protocol channel. Anything else on it means framing is lost,
/// and a decoder that has lost framing can answer the wrong call.
#[tokio::test]
async fn junk_on_stdout_is_fatal_rather_than_skipped() {
    let runner = runner();
    let dir = tempfile::tempdir().unwrap();
    let script = fake_host(
        dir.path(),
        "process.stdout.write('not a frame\\n');\nsetTimeout(() => {}, 60_000);\n",
    );

    let error = PluginHostSupervisor::start(config(&runner, script))
        .await
        .expect_err("a host that pollutes stdout is unusable");
    let rendered = error.to_string();
    assert!(
        rendered.contains("not a protocol frame"),
        "the failure must name the cause: {rendered}"
    );
}

/// A host that never answers is the one failure mode a caller cannot cancel,
/// so the handshake is the one call with a clock on it.
#[tokio::test]
async fn a_silent_host_fails_the_handshake_instead_of_hanging() {
    let runner = runner();
    let dir = tempfile::tempdir().unwrap();
    let script = fake_host(dir.path(), "setTimeout(() => {}, 60_000);\n");

    let started = std::time::Instant::now();
    let error = PluginHostSupervisor::start(
        config(&runner, script).with_startup_timeout(Duration::from_millis(750)),
    )
    .await
    .expect_err("a silent host cannot initialize");
    assert!(error.to_string().contains("did not answer"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the handshake must not wait on a host that never answers"
    );
}

/// A host that never became usable reports why, and says so as a host failure
/// rather than as the host having answered.
#[tokio::test]
async fn a_failed_start_reports_a_host_failure_not_a_rejection() {
    let runner = runner();
    let dir = tempfile::tempdir().unwrap();
    let script = fake_host(dir.path(), "process.stdout.write('not a frame\\n');\n");
    match PluginHostSupervisor::start(config(&runner, script)).await {
        Err(SupervisorError::Call(HostCallError::HostFailed(failure))) => {
            assert!(failure.reason.contains("not a protocol frame"), "{failure}");
        }
        Err(other) => panic!("expected a host failure, got {other}"),
        Ok(supervisor) => panic!("a host that pollutes stdout must not start: {supervisor:?}"),
    }
}

/// A restart is a new process on a new epoch, and the old one is really gone.
#[tokio::test]
async fn restarting_makes_a_new_epoch_on_a_fresh_process() {
    let runner = runner();
    let supervisor = PluginHostSupervisor::start(config(&runner, real_host()))
        .await
        .unwrap();
    supervisor
        .open_scope("plugin.a", "session-1", "C:/workspace")
        .await
        .unwrap();

    let owed = supervisor.restart().await.expect("restart succeeds");
    assert!(owed.is_empty(), "nothing was in flight");
    assert_eq!(supervisor.host_epoch().await, 2);
    assert!(supervisor.is_alive().await);

    // The new host knows nothing about the old host's scopes, so the same scope
    // opens again as a first incarnation rather than resuming.
    let generation = supervisor
        .open_scope("plugin.a", "session-1", "C:/workspace")
        .await
        .unwrap();
    assert_eq!(generation, 0);
    supervisor.shutdown().await.unwrap();
}

/// A host that answers `platform/initialize`, then asks rebon one question and
/// reports the answer on its way out.
///
/// Reporting through stderr is not a trick: a failure carries the host's stderr
/// tail, so this is the plainest way to observe what the host received without
/// teaching the supervisor a test-only channel.
fn upstream_probe(method: &str, payload: &str) -> String {
    format!(
        r#"
// The frame separator is spelled without an escape so that it survives being
// embedded in a Rust string that is itself embedded in a test file.
const NL = String.fromCharCode(10);
let buffer = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {{
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf(NL)) >= 0) {{
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    const frame = JSON.parse(line);
    if (frame.message.type === 'request' && frame.message.method === 'platform/initialize') {{
      process.stdout.write(JSON.stringify({{ ...frame, message: {{ type: 'terminal', status: 'success', payload: null }} }}) + NL);
      process.stdout.write(JSON.stringify({{ protocol_version: 1, host_epoch: frame.host_epoch,
        plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id: 'up-1',
        message: {{ type: 'request', method: '{method}', payload: {payload} }} }}) + NL);
    }} else if (frame.message.type === 'terminal' && frame.call_id === 'up-1') {{
      process.stderr.write('upstream answer ' + JSON.stringify(frame.message), () => process.exit(7));
    }}
  }}
}});
"#
    )
}

async fn diagnostics_after_probe(runner: &std::path::Path, method: &str, payload: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_host(dir.path(), &upstream_probe(method, payload));
    let supervisor = PluginHostSupervisor::start(config(runner, script))
        .await
        .expect("the probe host initializes");

    // The probe exits once it has its answer, so waiting for the host to be
    // gone is waiting for the answer to have arrived.
    for _ in 0..200 {
        if let Some(failure) = supervisor.failure().await {
            return failure.diagnostics.unwrap_or_default();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the probe host never reported an answer");
}

/// The upward direction's headline rule. A plugin that called a method this
/// build does not implement is waiting on a promise; dropping the request would
/// hang it with no diagnosis, so the refusal names the method instead.
#[tokio::test]
async fn an_upstream_request_this_supervisor_cannot_serve_is_still_answered() {
    let runner = runner();
    // Every method in the protocol's table is now implemented, so the probe
    // deliberately asks for a name outside it: what is under test is the rule
    // that an unhandled request is still *answered*, not any one method's
    // absence.
    let diagnostics = diagnostics_after_probe(&runner, "rebon/nonexistent", "null").await;
    assert!(
        diagnostics.contains("[UNSUPPORTED_METHOD]"),
        "the refusal must be a real answer: {diagnostics}"
    );
    assert!(
        diagnostics.contains("rebon/nonexistent"),
        "and it must name what was asked for: {diagnostics}"
    );
}

/// A payload that does not match the method's schema is refused as a payload
/// problem rather than as an unknown method: the two send an author to
/// different places.
#[tokio::test]
async fn an_upstream_request_with_a_bad_payload_is_refused_by_shape() {
    let runner = runner();
    let diagnostics =
        diagnostics_after_probe(&runner, "event/subscribe", r#"{"subscription": "only"}"#).await;
    assert!(
        diagnostics.contains("[MALFORMED_PAYLOAD]"),
        "expected a shape refusal: {diagnostics}"
    );
}
