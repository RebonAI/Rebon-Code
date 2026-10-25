//! A Verve host as the supervisor sees it: three pipes and an exit, driven
//! here by a stand-in `cli.mjs` that exports what the entry imports, so each
//! case can make the host behave as it needs.
//!
//! The real host on Verve is driven end to end by the supervisor's own suites
//! (`REBON_TEST_ENGINE=verve`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rebon_verve_host::{spawn, HostExit, VerveHost, VerveHostSpec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A `cli.mjs` whose `runHost` is `body`.
fn host(dir: &Path, body: &str) -> PathBuf {
    let script = dir.join("cli.mjs");
    let text = format!(
        r#"
export function diagnostic(stream, code, message) {{
  stream.write(JSON.stringify({{ level: 'error', code, message }}) + '\n');
}}
export function installRejectionDiagnostic() {{}}
export async function resolveLoader(specifier) {{ return {{ specifier }}; }}
export async function runHost({{ input, output, error, loader }}) {{
{body}
}}
"#
    );
    std::fs::write(&script, text).unwrap();
    script
}

fn started(script: PathBuf, environment: Option<Vec<(&str, &str)>>) -> VerveHost {
    spawn(VerveHostSpec {
        host_script: script.clone(),
        host_args: vec!["--loader".into(), "the-loader.mjs".into()],
        environment: environment.map(|vars| {
            vars.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect()
        }),
        working_directory: script.parent().unwrap().to_path_buf(),
        name: "test-host".into(),
        network: verve_permissions(),
    })
    .expect("the isolate starts")
}

async fn read_all(mut pipe: tokio::io::DuplexStream) -> String {
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(20), pipe.read_to_string(&mut text))
        .await
        .expect("the pipe closes")
        .unwrap();
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn input_comes_in_output_goes_out_and_the_loops_code_is_the_exit() {
    let dir = tempfile::tempdir().unwrap();
    let script = host(
        dir.path(),
        r#"
  error.write(JSON.stringify({
    argv: process.argv.slice(2), env: Object.keys(process.env),
    cwd: process.cwd().replace(/\\/g, '/').split('/').pop(), loader: loader.specifier,
  }) + '\n');
  let total = 0;
  for await (const chunk of input) {
    total += chunk.length;
    output.write(new TextEncoder().encode(new TextDecoder().decode(chunk).toUpperCase()));
  }
  output.write(`total ${total}\n`);
  return 7;
"#,
    );
    let mut host = started(script, Some(vec![("ONLY_THIS", "1")]));
    host.stdin.write_all(b"hello ").await.unwrap();
    host.stdin.write_all(b"world\n").await.unwrap();
    drop(host.stdin);
    let stdout = read_all(host.stdout).await;
    let stderr = read_all(host.stderr).await;
    assert_eq!(stdout, "HELLO WORLD\ntotal 12\n");
    let first: serde_json::Value = serde_json::from_str(stderr.lines().next().unwrap()).unwrap();
    let dir_name = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        first,
        serde_json::json!({
            "argv": ["--loader", "the-loader.mjs"],
            "env": ["ONLY_THIS"],
            "cwd": dir_name,
            "loader": "the-loader.mjs",
        })
    );
    assert_eq!(host.process.wait().await, HostExit::Code(7));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_console_is_diagnostics_never_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let script = host(
        dir.path(),
        r#"
  console.log('from a plugin');
  process.stderr.write('raw stderr\n');
  output.write('frame\n');
  return 0;
"#,
    );
    let host = started(script, None);
    let stdout = read_all(host.stdout).await;
    let stderr = read_all(host.stderr).await;
    assert_eq!(stdout, "frame\n");
    assert!(stderr.contains("from a plugin"), "{stderr}");
    assert!(stderr.contains("raw stderr"), "{stderr}");
    assert!(host.process.wait().await.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_stuck_in_a_loop_is_killed_and_its_pipes_close() {
    let dir = tempfile::tempdir().unwrap();
    let script = host(
        dir.path(),
        r#"
  output.write('started\n');
  await new Promise((resolve) => setTimeout(resolve, 10));
  for (;;) {}
"#,
    );
    let mut host = started(script, None);
    let mut first = [0u8; 8];
    tokio::time::timeout(Duration::from_secs(20), host.stdout.read_exact(&mut first))
        .await
        .expect("the host starts")
        .unwrap();
    assert_eq!(&first, b"started\n");
    // Long enough for the timer to fire and the loop to be running.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(host.process.try_exit(), None);
    host.process.start_kill();
    let exit = tokio::time::timeout(Duration::from_secs(10), host.process.wait())
        .await
        .expect("a kill does not wait for the script");
    assert_eq!(exit, HostExit::Killed);
    let rest = read_all(host.stdout).await;
    assert_eq!(rest, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_whose_imports_fail_says_so_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cli.mjs");
    std::fs::write(&script, "export function diagnostic( {\n").unwrap();
    let host = started(script, None);
    let stdout = read_all(host.stdout).await;
    let stderr = read_all(host.stderr).await;
    assert_eq!(stdout, "");
    let line: serde_json::Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(line["code"], "host_failure", "{stderr}");
    assert_eq!(host.process.wait().await, HostExit::Code(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loader_that_cannot_be_built_is_not_replaced_by_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cli.mjs");
    std::fs::write(
        &script,
        r#"
export function diagnostic(stream, code, message) {
  stream.write(JSON.stringify({ level: 'error', code, message }) + '\n');
}
export function installRejectionDiagnostic() {}
export async function resolveLoader() {
  const error = new Error('no such loader'); error.code = 'loader_invalid'; throw error;
}
export async function runHost() { throw new Error('must not run'); }
"#,
    )
    .unwrap();
    let host = started(script, None);
    let stderr = read_all(host.stderr).await;
    assert!(stderr.contains("loader_invalid"), "{stderr}");
    assert!(!stderr.contains("must not run"), "{stderr}");
    assert_eq!(host.process.wait().await, HostExit::Code(2));
}

/// The test hosts touch no network.
fn verve_permissions() -> verve::Permissions {
    verve::Permissions::default()
}
