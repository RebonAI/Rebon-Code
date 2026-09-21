use super::*;

pub(crate) fn drain_pending_updates(
    app: &mut AppState,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdateParams>,
) -> usize {
    use tokio::sync::mpsc::error::TryRecvError;

    let mut drained = 0usize;
    loop {
        match rx.try_recv() {
            Ok(params) => {
                translate_session_update(app, params);
                drained += 1;
            }
            Err(TryRecvError::Empty) => {
                if drained > 0 {
                    tracing::debug!(
                        target: "stream_dbg",
                        drained,
                        overlay_blocks = app.rebon_tui.overlay.blocks.len(),
                        transcript_rows = app.rebon_tui.transcript.rows().len(),
                        pending_perm = app.pending_permission_view.is_some(),
                        "tui: drain_pending_updates done"
                    );
                }
                return drained;
            }
            Err(TryRecvError::Disconnected) => {
                tracing::debug!("rebon-cli: session update channel disconnected; no more updates");
                return drained;
            }
        }
    }
}

pub(in crate::tui::runner) const FILE_LIST_DRAIN_UPDATE_BUDGET: usize = 8;
pub(in crate::tui::runner) const FILE_LIST_DRAIN_PATH_BUDGET: usize = 4096;

/// Drain file list updates from the async file scanner into the
/// file index. Called each frame so new files become searchable as
/// soon as root seed/chunked scanner batches arrive.
pub(in crate::tui::runner) fn drain_file_list(
    app: &mut AppState,
    rx: &mut crate::file_scanner::FileListRx,
) {
    use crate::file_scanner::{FileListUpdate, FileScanStatus};
    use tokio::sync::mpsc::error::TryRecvError;

    let mut updates_drained = 0usize;
    let mut paths_drained = 0usize;
    loop {
        if updates_drained >= FILE_LIST_DRAIN_UPDATE_BUDGET
            || paths_drained >= FILE_LIST_DRAIN_PATH_BUDGET
        {
            tracing::debug!(
                updates_drained,
                paths_drained,
                "drain_file_list: per-frame budget reached"
            );
            return;
        }

        match rx.try_recv() {
            Ok(update) => {
                updates_drained += 1;
                match update {
                    FileListUpdate::Seed { directories, files } => {
                        tracing::debug!(
                            directories = directories.len(),
                            files = files.len(),
                            "drain_file_list: merging root seed"
                        );
                        paths_drained += directories.len() + files.len();
                        app.file_index.merge_directories(directories);
                        app.file_index.merge(files);
                    }
                    FileListUpdate::Tracked(paths) => {
                        tracing::debug!(
                            count = paths.len(),
                            "drain_file_list: merging tracked files"
                        );
                        paths_drained += paths.len();
                        app.file_index.merge(paths);
                    }
                    FileListUpdate::Untracked(paths) => {
                        tracing::debug!(
                            count = paths.len(),
                            "drain_file_list: merging untracked files"
                        );
                        paths_drained += paths.len();
                        app.file_index.merge(paths);
                    }
                    FileListUpdate::Prefix(paths) => {
                        tracing::debug!(
                            count = paths.len(),
                            "drain_file_list: merging prefix scan files"
                        );
                        paths_drained += paths.len();
                        app.file_index.merge(paths);
                    }
                    FileListUpdate::Complete => {
                        tracing::debug!("drain_file_list: scanner complete");
                        app.file_scan_status = FileScanStatus::Complete;
                    }
                    FileListUpdate::Failed(reason) => {
                        tracing::debug!(reason, "drain_file_list: scanner failed");
                        if app.file_scan_status != FileScanStatus::Complete {
                            app.file_scan_status = FileScanStatus::Failed(reason);
                        }
                    }
                    FileListUpdate::TimedOut(phase) => {
                        tracing::debug!(phase, "drain_file_list: scanner timed out");
                        if app.file_scan_status != FileScanStatus::Complete {
                            app.file_scan_status = FileScanStatus::TimedOut(phase);
                        }
                    }
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

/// Shorten a cwd path for display: show last two path components.
/// e.g. "/home/user/projects/rebon" → "projects/rebon"
pub(in crate::tui::runner) fn shorten_cwd(cwd: &str) -> String {
    let path = std::path::Path::new(cwd);
    let components: Vec<_> = path.components().collect();
    if components.len() <= 2 {
        return cwd.replace('\\', "/");
    }
    components[components.len() - 2..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub(in crate::tui::runner) fn note_cancel_race() {
    tracing::debug!(
        "rebon-cli: cancel is optimistic; late updates from the cancelled turn may still arrive because ACP updates are session-scoped, not turn-scoped"
    );
}
