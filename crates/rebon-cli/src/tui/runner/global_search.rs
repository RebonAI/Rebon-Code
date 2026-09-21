//! Global search dialog runtime: applies dialog actions and owns the
//! background ripgrep worker used to populate search results.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};

use rebon_dialog::global_search::GlobalSearchAction;

use crate::tui::app::AppState;
use crate::tui::external_editor::open_file_in_external_editor;
use crate::tui::global_search_dialog::SearchRequest;

use super::quick_open::insert_with_spacing;

pub(super) struct GlobalSearchWorker {
    generation: u64,
    rx: Receiver<GlobalSearchWorkerResult>,
}

struct GlobalSearchWorkerResult {
    generation: u64,
    matches: Vec<rebon_dialog::global_search::GlobalSearchMatch>,
    truncated: bool,
    failed_reason: Option<String>,
}

pub(super) fn apply_global_search_action(
    app: &mut AppState,
    cwd: &Path,
    action: GlobalSearchAction,
) {
    match action {
        GlobalSearchAction::OpenInEditor { file, line } => {
            let full = if Path::new(&file).is_absolute() {
                PathBuf::from(file)
            } else {
                cwd.join(file)
            };
            let _ = open_file_in_external_editor(&full, Some(line));
        }
        GlobalSearchAction::InsertMention { text } | GlobalSearchAction::InsertPath { text } => {
            insert_with_spacing(app, &text);
        }
        GlobalSearchAction::Cancel => {}
    }
}

pub(super) fn sync_global_search_dialog(
    app: &mut AppState,
    cwd: &Path,
    worker: &mut Option<GlobalSearchWorker>,
) {
    let Some(dialog) = app.global_search_dialog.as_mut() else {
        *worker = None;
        return;
    };

    if let Some(request) = dialog.maybe_take_search_request() {
        *worker = Some(spawn_global_search_worker(request, cwd.to_path_buf()));
    }

    let Some(active) = worker.as_ref() else {
        return;
    };

    match active.rx.try_recv() {
        Ok(result) => {
            if let Some(reason) = result.failed_reason {
                dialog.apply_search_failure(result.generation, reason);
            } else {
                dialog.apply_search_results(
                    result.generation,
                    result.matches,
                    result.truncated,
                    cwd,
                );
            }
            *worker = None;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            dialog.apply_search_failure(
                active.generation,
                "Search failed: background worker stopped before returning results.",
            );
            *worker = None;
        }
    }
}

fn spawn_global_search_worker(request: SearchRequest, cwd: PathBuf) -> GlobalSearchWorker {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_global_search_worker(request.generation, &request.query, &cwd);
        let _ = tx.send(result);
    });
    GlobalSearchWorker {
        generation: request.generation,
        rx,
    }
}

fn run_global_search_worker(generation: u64, query: &str, cwd: &Path) -> GlobalSearchWorkerResult {
    let per_file_limit = rebon_dialog::global_search::MAX_MATCHES_PER_FILE.to_string();
    let ripgrep = match crate::ripgrep::resolve_ripgrep_command() {
        Ok(ripgrep) => ripgrep,
        Err(err) => {
            tracing::warn!(error = %err, query = %query, "rebon-cli: global search rg not found");
            return GlobalSearchWorkerResult {
                generation,
                matches: Vec::new(),
                truncated: false,
                failed_reason: Some(format!(
                    "Search unavailable: {err} Global text search requires rg; install ripgrep or set REBON_RIPGREP_PATH."
                )),
            };
        }
    };
    let mut command = Command::new(&ripgrep.program);
    tracing::debug!(program = %ripgrep.program.display(), mode = ?ripgrep.mode, "rebon-cli: global search using ripgrep");
    command
        .current_dir(cwd)
        .args([
            "-n",
            "--no-heading",
            "-i",
            "-m",
            per_file_limit.as_str(),
            "-F",
            "-e",
            query,
            ".",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let output = match command.output() {
        Ok(output) => output,
        Err(err) => {
            tracing::warn!(error = %err, query = %query, "rebon-cli: global search spawn failed");
            return GlobalSearchWorkerResult {
                generation,
                matches: Vec::new(),
                truncated: false,
                failed_reason: Some(format!(
                    "Search unavailable: failed to start rg ({err}). Install ripgrep or set REBON_RIPGREP_PATH."
                )),
            };
        }
    };

    if !(output.status.success() || output.status.code() == Some(1)) {
        tracing::warn!(
            status = ?output.status.code(),
            query = %query,
            "rebon-cli: global search rg exited with error"
        );
        return GlobalSearchWorkerResult {
            generation,
            matches: Vec::new(),
            truncated: false,
            failed_reason: Some(
                "Search unavailable: rg exited with an error. Install ripgrep or set REBON_RIPGREP_PATH if rg is missing or broken."
                    .to_string(),
            ),
        };
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches = Vec::new();
    for line in stdout.lines() {
        let Some(mut parsed) = rebon_dialog::global_search::parse_ripgrep_line(line) else {
            continue;
        };
        parsed.file = parsed.file.replace('\\', "/");
        matches.push(parsed);
        if matches.len() >= rebon_dialog::global_search::MAX_TOTAL_MATCHES {
            break;
        }
    }
    let truncated = stdout.lines().count() > matches.len()
        || matches.len() >= rebon_dialog::global_search::MAX_TOTAL_MATCHES;

    GlobalSearchWorkerResult {
        generation,
        matches,
        truncated,
        failed_reason: None,
    }
}
