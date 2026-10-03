use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread")]
async fn exec_input_errors_precede_config_and_runtime_startup() {
    let _config_home = rebon_tool::tasks::test_support::TestConfigHome::new("exec-prompt-input");
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("config-home");
    let empty = dir.path().join("empty.txt");
    let invalid = dir.path().join("invalid.txt");
    let missing = dir.path().join("missing.txt");
    std::fs::write(&empty, "\u{feff} \r\n\t").unwrap();
    std::fs::write(&invalid, [0xef, 0xbb, 0xbf, 0xff]).unwrap();

    let cases = [
        (Vec::new(), Vec::new(), "required"),
        (vec![" \t\n".to_string()], Vec::new(), "empty prompt"),
        (
            vec!["--prompt-file".into(), empty.display().to_string()],
            Vec::new(),
            "empty prompt",
        ),
        (
            vec!["--prompt-file".into(), invalid.display().to_string()],
            Vec::new(),
            "not valid UTF-8",
        ),
        (
            vec!["--prompt-file".into(), missing.display().to_string()],
            Vec::new(),
            "failed to read prompt file",
        ),
        (
            vec!["--prompt-file".into(), dir.path().display().to_string()],
            Vec::new(),
            "failed to read prompt file",
        ),
        (
            vec!["--prompt-file".into(), "-".into()],
            Vec::new(),
            "empty prompt",
        ),
        (
            vec!["--prompt-file".into(), "-".into()],
            b"\xef\xbb\xbf \r\n".to_vec(),
            "empty prompt",
        ),
        (
            vec!["--prompt-file".into(), "-".into()],
            vec![0xff],
            "stdin is not valid UTF-8",
        ),
        (
            vec!["--prompt-file".into(), "-".into(), "conflict".into()],
            Vec::new(),
            "cannot be used with",
        ),
    ];
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_rebon"))
        .canonicalize()
        .unwrap();
    for (args, input, expected) in cases {
        for json in [false, true] {
            let mut child = Command::new(&binary)
                .arg("exec")
                .args(json.then_some("--json"))
                .args(&args)
                .current_dir(dir.path())
                .env("REBON_CONFIG_DIR", &home)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(&input).await.unwrap();
            drop(stdin);
            let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
                .await
                .expect("输入错误不应等待运行时启动")
                .unwrap();
            assert!(!output.status.success(), "{args:?}");
            if json && expected != "required" && expected != "cannot be used with" {
                let events: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(events.len(), 1, "{args:?}: {events:?}");
                assert_eq!(events[0]["type"], "error");
                assert_eq!(events[0].as_object().unwrap().len(), 2);
                assert!(events[0]["message"].as_str().unwrap().contains(expected));
            } else {
                assert!(output.stdout.is_empty(), "{args:?}: {:?}", output.stdout);
            }
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stderr.contains(expected), "{args:?}: {stderr}");
            assert!(!home.exists(), "输入错误不应创建配置目录: {args:?}");
        }
    }
}
