//! Local Remote Control runner status, hosted by the shared dialog stack.

use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use rebon_dialog::model::{
    DialogKey, DialogModel, DialogOutcome, KeyPress, PanelPane, PanelRow, PanelView, TextSpan,
    ViewSpec,
};
use rebon_rc_runner::login::Status;

use crate::tui::app::AppState;

// The runner changes these files and its process lock independently of the TUI.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const READ_STALL_THRESHOLD: Duration = Duration::from_secs(10);

type StatusResult = Result<Option<Arc<Status>>, String>;

#[derive(Debug, Clone, Default)]
pub struct RcStatusState {
    pub status: Option<Arc<Status>>,
    pub error: Option<String>,
}

impl RcStatusState {
    pub fn is_visible(&self) -> bool {
        self.status.is_some() || self.error.is_some()
    }

    fn apply(&mut self, result: StatusResult) {
        match result {
            Ok(status) => {
                self.status = status;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
}

struct PendingStatus {
    id: u64,
    receiver: mpsc::Receiver<(u64, StatusResult)>,
    started: Instant,
    stalled: bool,
}

#[derive(Default)]
pub struct RcStatusRuntime {
    next_id: u64,
    pending: Option<PendingStatus>,
    last_started: Option<Instant>,
}

impl RcStatusRuntime {
    pub fn sync(&mut self, app: &mut AppState, cwd: &Path, now: Instant) {
        let mut changed = self.drain(&mut app.rc_status, now);
        if self.pending.is_none()
            && self
                .last_started
                .is_none_or(|started| now.duration_since(started) >= REFRESH_INTERVAL)
        {
            self.next_id += 1;
            let id = self.next_id;
            let cwd = cwd.to_path_buf();
            let (sender, receiver) = mpsc::channel();
            self.last_started = Some(now);
            match std::thread::Builder::new()
                .name("rc-status".into())
                .spawn(move || {
                    let result = read_status(&cwd).map_err(|error| format!("{error:#}"));
                    // Closing the TUI drops the receiver before the local read finishes.
                    let _ = sender.send((id, result));
                }) {
                Ok(_) => {
                    self.pending = Some(PendingStatus {
                        id,
                        receiver,
                        started: now,
                        stalled: false,
                    });
                }
                Err(error) => {
                    app.rc_status
                        .apply(Err(format!("Cannot read RC status: {error}")));
                    changed = true;
                }
            }
        }
        if changed {
            if let Some(dialog) = app.dialogs.top_as_mut::<BridgeDialogState>() {
                dialog.status = app.rc_status.clone();
            }
        }
    }

    fn drain(&mut self, status: &mut RcStatusState, now: Instant) -> bool {
        let Some(pending) = &mut self.pending else {
            return false;
        };
        match pending.receiver.try_recv() {
            Ok((id, result)) => {
                let current = id == pending.id;
                self.pending = None;
                if current {
                    status.apply(result);
                }
                current
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                status.apply(Err("RC status reader stopped without a result".into()));
                true
            }
            Err(mpsc::TryRecvError::Empty) => {
                if !pending.stalled && now.duration_since(pending.started) >= READ_STALL_THRESHOLD {
                    pending.stalled = true;
                    status.apply(Err("RC status read is still waiting on local files".into()));
                    // Keep the reader: replacing a blocked file-lock read would leak threads.
                    return true;
                }
                false
            }
        }
    }
}

fn read_status(cwd: &Path) -> anyhow::Result<Option<Arc<Status>>> {
    let dir = rebon_rc_runner::files::RcDir::new(&rebon_config::config_home_dir());
    if !dir.credentials_path().try_exists()? {
        return Ok(None);
    }
    let projects = rebon_config::load_rc_projects().and_then(|projects| {
        let configured = projects
            .into_iter()
            .map(|project| rebon_rc_runner::projects::ConfiguredProject {
                path: project.path,
                label: project.label,
            })
            .collect();
        rebon_rc_runner::projects::resolve_projects(configured, &[], cwd)
    });
    rebon_rc_runner::login::status(&dir, &crate::background::cli_default_store(), projects)
        .map(|status| Some(Arc::new(status)))
}

#[derive(Debug, Clone)]
pub struct BridgeDialogState {
    status: RcStatusState,
    scroll: u16,
    viewport_width: u16,
}

impl BridgeDialogState {
    pub fn new(status: RcStatusState) -> Self {
        Self {
            status,
            scroll: 0,
            viewport_width: 0,
        }
    }

    fn body(&self) -> PanelPane {
        PanelPane {
            rows: self.rows(),
            scroll: self.scroll,
            wrap: true,
            ..Default::default()
        }
    }

    fn last_line(&self) -> u16 {
        rebon_tui::dialog_view::panel_content_height(&self.body(), self.viewport_width)
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16
    }

    fn rows(&self) -> Vec<PanelRow> {
        let mut rows = vec![PanelRow::one(TextSpan::strong("Local runner status"))];
        if let Some(error) = &self.status.error {
            rows.push(PanelRow::one(TextSpan::strong(format!(
                "Refresh failed: {error}"
            ))));
            if self.status.status.is_some() {
                rows.push(PanelRow::one(TextSpan::strong(
                    "Last successful snapshot (stale):",
                )));
            }
        }
        if let Some(status) = &self.status.status {
            rows.extend(
                rebon_rc_runner::login::render_status(status)
                    .lines()
                    .map(|line| PanelRow::one(TextSpan::normal(line))),
            );
        } else if self.status.error.is_none() {
            rows.push(PanelRow::one(TextSpan::normal(
                "Not configured. Run rebon rc login --server <url>, then rebon rc serve.",
            )));
        }
        rows.push(PanelRow::blank());
        rows.push(PanelRow::one(TextSpan::dim(
            "Process status is local; server connectivity is not checked.",
        )));
        rows.push(PanelRow::one(TextSpan::dim(
            "Projects are resolved from config and this directory, not live --project flags.",
        )));
        rows.push(PanelRow::one(TextSpan::dim(
            "Sessions are remembered mappings, not a list of active connections.",
        )));
        rows.push(PanelRow::one(TextSpan::dim(
            "Opening this panel does not share the foreground session.",
        )));
        rows.push(PanelRow::one(TextSpan::dim(
            "Start with rebon rc serve; stop with Ctrl+C in that runner's terminal.",
        )));
        rows.push(PanelRow::one(TextSpan::dim(
            "The RC server can read prompts, tool output and transcripts; no end-to-end encryption.",
        )));
        rows
    }
}

impl DialogModel for BridgeDialogState {
    rebon_dialog::dialog_plumbing!();

    fn id(&self) -> &'static str {
        "bridge"
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        let last = self.last_line();
        match press.key {
            DialogKey::Enter | DialogKey::Escape => return DialogOutcome::Close,
            DialogKey::Up => self.scroll = self.scroll.saturating_sub(1),
            DialogKey::Down => self.scroll = self.scroll.saturating_add(1).min(last),
            DialogKey::PageUp => {
                self.scroll = self.scroll.saturating_sub(press.viewport_rows_or(10));
            }
            DialogKey::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_add(press.viewport_rows_or(10))
                    .min(last);
            }
            DialogKey::Home => self.scroll = 0,
            DialogKey::End => self.scroll = last,
            _ => {}
        }
        DialogOutcome::None
    }

    fn note_viewport(&mut self, _rows: u16, cols: u16) {
        self.viewport_width = cols;
        self.scroll = self.scroll.min(self.last_line());
    }

    fn view(&self) -> ViewSpec {
        ViewSpec::Panel(PanelView {
            title: format!(" {} ", rebon_dialog::bridge_dialog::TITLE),
            body: self.body(),
            footer: vec![TextSpan::dim(
                "Up/Down/PgUp/PgDn scroll · Enter/Esc close · refresh 5s",
            )],
            desired_height: Some(24),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(pid: Option<&str>) -> Arc<Status> {
        Arc::new(Status {
            logged_in: true,
            server: Some("https://rc.example.com".into()),
            account_id: Some("account-1".into()),
            device_id: Some("device-1".into()),
            environment_id: Some("environment-1".into()),
            client_environment_id: Some("client-1".into()),
            serving_pid: pid.map(str::to_owned),
            projects: Vec::new(),
            projects_error: None,
            sessions: Default::default(),
        })
    }

    fn text(dialog: &BridgeDialogState) -> String {
        dialog
            .rows()
            .iter()
            .map(PanelRow::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn status_visibility_tracks_configuration_and_read_errors() {
        let mut state = RcStatusState::default();
        assert!(!state.is_visible());
        state.apply(Err("unreadable credentials".into()));
        assert!(state.is_visible());
        state.apply(Ok(Some(snapshot(None))));
        assert!(state.is_visible());
        assert!(state.error.is_none());
        state.apply(Ok(None));
        assert!(!state.is_visible());
    }

    #[test]
    fn running_process_is_not_reported_as_a_server_connection() {
        let dialog = BridgeDialogState::new(RcStatusState {
            status: Some(snapshot(Some("42"))),
            error: None,
        });
        let output = text(&dialog);
        for expected in [
            "running (pid 42)",
            "server connectivity is not checked",
            "does not share the foreground session",
            "no end-to-end encryption",
        ] {
            assert!(output.contains(expected), "{output}");
        }
        assert!(!output.contains("Remote Control active"));
    }

    #[test]
    fn stopped_runner_remains_visible_and_offers_explicit_start_command() {
        let state = RcStatusState {
            status: Some(snapshot(None)),
            error: None,
        };
        assert!(state.is_visible());
        let output = text(&BridgeDialogState::new(state));
        assert!(output.contains("not running"));
        assert!(output.contains("rebon rc serve"));
    }

    #[test]
    fn failed_refresh_preserves_and_marks_the_last_snapshot() {
        let mut state = RcStatusState {
            status: Some(snapshot(Some("42"))),
            error: None,
        };
        state.apply(Err("ledger unreadable".into()));
        let output = text(&BridgeDialogState::new(state));
        assert!(output.contains("ledger unreadable"));
        assert!(output.contains("snapshot (stale)"));
        assert!(output.contains("pid 42"));
    }

    #[test]
    fn close_and_former_disconnect_keys_have_no_remote_side_effects() {
        let mut dialog = BridgeDialogState::new(RcStatusState::default());
        for key in [DialogKey::plain('d'), DialogKey::plain(' ')] {
            assert_eq!(dialog.on_key(key.into()), DialogOutcome::None);
        }
        for key in [DialogKey::Enter, DialogKey::Escape] {
            assert_eq!(dialog.on_key(key.into()), DialogOutcome::Close);
        }
    }

    #[test]
    fn pending_read_is_nonblocking_and_stall_does_not_spawn_a_replacement() {
        let now = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let mut runtime = RcStatusRuntime {
            next_id: 1,
            last_started: Some(now),
            pending: Some(PendingStatus {
                id: 1,
                receiver,
                started: now,
                stalled: false,
            }),
        };
        let mut state = RcStatusState::default();
        assert!(!runtime.drain(&mut state, now));
        assert!(runtime.drain(&mut state, now + READ_STALL_THRESHOLD));
        assert!(runtime.pending.is_some());
        assert!(state.error.as_ref().unwrap().contains("still waiting"));
        assert!(!runtime.drain(&mut state, now + READ_STALL_THRESHOLD));
        sender.send((1, Ok(Some(snapshot(None))))).unwrap();
        assert!(runtime.drain(&mut state, now + READ_STALL_THRESHOLD));
        assert!(runtime.pending.is_none());
        assert!(state.error.is_none());
    }

    #[test]
    fn outdated_read_result_is_discarded() {
        let now = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let mut runtime = RcStatusRuntime {
            next_id: 2,
            last_started: Some(now),
            pending: Some(PendingStatus {
                id: 2,
                receiver,
                started: now,
                stalled: false,
            }),
        };
        sender.send((1, Ok(Some(snapshot(Some("99")))))).unwrap();
        let mut state = RcStatusState::default();
        assert!(!runtime.drain(&mut state, now));
        assert!(!state.is_visible());
    }

    #[test]
    fn refresh_keeps_the_open_dialog_scroll_position() {
        let now = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let mut runtime = RcStatusRuntime {
            next_id: 1,
            last_started: Some(now),
            pending: Some(PendingStatus {
                id: 1,
                receiver,
                started: now,
                stalled: false,
            }),
        };
        let mut app = AppState::new();
        let mut dialog = BridgeDialogState::new(RcStatusState::default());
        dialog.scroll = 3;
        app.dialogs.push(dialog);
        sender.send((1, Ok(Some(snapshot(Some("42")))))).unwrap();
        runtime.sync(&mut app, Path::new("unused"), now);
        let dialog = app.dialogs.top_as_mut::<BridgeDialogState>().unwrap();
        assert_eq!(dialog.scroll, 3);
        assert!(text(dialog).contains("pid 42"));
    }

    #[test]
    fn end_reaches_beyond_logical_rows_in_a_narrow_panel() {
        let mut dialog = BridgeDialogState::new(RcStatusState::default());
        dialog.note_viewport(4, 12);
        dialog.on_key(DialogKey::End.into());
        assert!(usize::from(dialog.scroll) > dialog.rows().len());
    }

    fn configured_home() -> (
        rebon_tool::tasks::test_support::TestConfigHome,
        rebon_rc_runner::files::RcDir,
    ) {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("rc-tui");
        let dir = rebon_rc_runner::files::RcDir::new(home.path());
        dir.save_credentials(&rebon_rc_runner::files::StoredCredentials {
            server: "http://127.0.0.1:9".into(),
            account_id: "account-1".into(),
            device_id: "device-1".into(),
            refresh_token: "secret-not-for-display".into(),
            created_at_ms: 1,
        })
        .unwrap();
        (home, dir)
    }

    #[test]
    fn unconfigured_status_does_not_create_rc_files() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("rc-absent");
        assert!(read_status(home.path()).unwrap().is_none());
        assert!(!home.path().join("rc").exists());
    }

    #[test]
    fn local_status_reports_runner_lifecycle_and_remembered_sessions_without_secrets() {
        let (home, dir) = configured_home();
        let mut identity = rebon_rc_runner::files::EnvironmentIdentity::fresh("http://127.0.0.1:9");
        identity.environment_id = Some("environment-1".into());
        dir.save_environment(&identity).unwrap();
        rebon_rc_runner::ledger::Ledger::new(dir.clone())
            .record_session(
                "remote-1",
                rebon_rc_runner::ledger::SessionEntry {
                    rebon_session_id: "local-1".into(),
                    project: home.path().display().to_string(),
                    cwd: home.path().display().to_string(),
                    job_id: None,
                    environment_id: "environment-1".into(),
                    updated_at_ms: 1,
                },
            )
            .unwrap();
        let lock = dir.lock_serve().unwrap();
        let status = read_status(home.path()).unwrap().unwrap();
        assert_eq!(status.serving_pid, Some(std::process::id().to_string()));
        assert_eq!(status.environment_id.as_deref(), Some("environment-1"));
        assert_eq!(status.projects.len(), 1);
        assert_eq!(
            status.sessions["remote-1"].entry.rebon_session_id,
            "local-1"
        );
        let output = text(&BridgeDialogState::new(RcStatusState {
            status: Some(status),
            error: None,
        }));
        assert!(output.contains("remote-1 -> local-1"));
        assert!(output.contains("remembered mappings"));
        assert!(!output.contains("secret-not-for-display"));
        drop(lock);
        assert!(read_status(home.path())
            .unwrap()
            .unwrap()
            .serving_pid
            .is_none());
    }

    #[test]
    fn corrupt_local_files_fail_visibly_and_recover_after_repair() {
        let (home, dir) = configured_home();
        for path in [
            dir.credentials_path(),
            dir.environment_path(),
            dir.ledger_path(),
        ] {
            let original = std::fs::read(&path).ok();
            std::fs::write(&path, "{").unwrap();
            let error = read_status(home.path()).unwrap_err();
            assert!(format!("{error:#}").contains(path.file_name().unwrap().to_str().unwrap()));
            match original {
                Some(bytes) => std::fs::write(&path, bytes).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
            assert!(read_status(home.path()).unwrap().is_some());
        }
    }

    #[test]
    fn configured_projects_and_resolution_errors_are_visible() {
        let (home, _) = configured_home();
        let project = home.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let config = home.path().join("config.json");
        std::fs::write(
            &config,
            r#"{"rc":{"projects":[{"path":"project","label":"Configured project"}]}}"#,
        )
        .unwrap();
        let status = read_status(home.path()).unwrap().unwrap();
        assert_eq!(status.projects.len(), 1);
        assert_eq!(status.projects[0].label, "Configured project");
        assert!(status.projects_error.is_none());
        for bad_config in [
            r#"{"rc":{"projects":42}}"#,
            r#"{"rc":{"projects":["missing-project"]}}"#,
        ] {
            std::fs::write(&config, bad_config).unwrap();
            let status = read_status(home.path()).unwrap().unwrap();
            assert!(status.projects_error.is_some());
            assert!(text(&BridgeDialogState::new(RcStatusState {
                status: Some(status),
                error: None
            }))
            .contains("cannot be resolved"));
        }
    }

    #[test]
    fn refresh_is_throttled_and_reads_local_status_off_the_ui_thread() {
        let (home, _) = configured_home();
        let now = Instant::now();
        let mut runtime = RcStatusRuntime {
            last_started: Some(now),
            ..Default::default()
        };
        let mut app = AppState::new();
        runtime.sync(&mut app, home.path(), now);
        assert!(runtime.pending.is_none());
        runtime.sync(&mut app, home.path(), now + REFRESH_INTERVAL);
        let pending = runtime.pending.as_ref().unwrap();
        let (id, result) = pending
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(id, pending.id);
        assert!(result.unwrap().unwrap().logged_in);
    }

    #[test]
    fn scrolling_is_bounded_and_resize_clamps_the_position() {
        let mut dialog = BridgeDialogState::new(RcStatusState::default());
        dialog.note_viewport(4, 12);
        dialog.on_key(DialogKey::Up.into());
        assert_eq!(dialog.scroll, 0);
        dialog.on_key(KeyPress {
            viewport_rows: Some(4),
            ..DialogKey::PageDown.into()
        });
        assert_eq!(dialog.scroll, 4);
        dialog.on_key(KeyPress {
            viewport_rows: Some(4),
            ..DialogKey::PageUp.into()
        });
        assert_eq!(dialog.scroll, 0);
        dialog.on_key(DialogKey::End.into());
        let narrow_end = dialog.scroll;
        dialog.on_key(DialogKey::Down.into());
        assert_eq!(dialog.scroll, narrow_end);
        dialog.note_viewport(20, 120);
        assert!(dialog.scroll < narrow_end);
        assert_eq!(dialog.scroll, dialog.last_line());
        dialog.on_key(DialogKey::Home.into());
        assert_eq!(dialog.scroll, 0);
    }

    #[test]
    fn screen_and_inline_panels_show_the_final_wrapped_line_of_long_status() {
        use crate::tui::dialog_host;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{backend::TestBackend, Terminal};
        for inline in [false, true] {
            for (width, height) in [(20, 8), (100, 24)] {
                let mut status = snapshot(None);
                for index in 0..60 {
                    Arc::get_mut(&mut status).unwrap().sessions.insert(
                        format!("remote-{index:03}"),
                        rebon_rc_runner::login::SessionStatus {
                            entry: rebon_rc_runner::ledger::SessionEntry {
                                rebon_session_id: format!("local-{index}"),
                                project: "project".into(),
                                cwd: "a/very/long/project/directory/with/many/components".into(),
                                job_id: None,
                                environment_id: "environment-1".into(),
                                updated_at_ms: 1,
                            },
                            job_state: Some("running".into()),
                        },
                    );
                }
                let mut host = dialog_host::DialogHost::default();
                host.push(BridgeDialogState::new(RcStatusState {
                    status: Some(status),
                    error: None,
                }));
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let draw = |terminal: &mut Terminal<TestBackend>,
                            host: &mut dialog_host::DialogHost| {
                    terminal
                        .draw(|frame| {
                            let area = frame.area();
                            if inline {
                                dialog_host::render_inline(host, frame, area);
                            } else {
                                dialog_host::render_screen(host, frame, area, area);
                            }
                        })
                        .unwrap();
                };
                draw(&mut terminal, &mut host);
                dialog_host::handle_key(
                    &mut host,
                    &KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                );
                draw(&mut terminal, &mut host);
                let output: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(
                    output.contains("encryption."),
                    "inline={inline} width={width}: {output}"
                );
                dialog_host::handle_key(
                    &mut host,
                    &KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                );
                draw(&mut terminal, &mut host);
                let output: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(output.contains("Local runner"), "{output}");
            }
        }
    }

    #[test]
    fn reader_exit_is_visible_instead_of_silently_hiding_status() {
        let now = Instant::now();
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        let mut runtime = RcStatusRuntime {
            next_id: 1,
            last_started: Some(now),
            pending: Some(PendingStatus {
                id: 1,
                receiver,
                started: now,
                stalled: false,
            }),
        };
        let mut state = RcStatusState::default();
        assert!(runtime.drain(&mut state, now));
        assert!(state.error.unwrap().contains("without a result"));
    }
}
