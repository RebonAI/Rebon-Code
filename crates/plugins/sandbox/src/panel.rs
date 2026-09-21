//! The `/sandbox` panel: three tabs over this crate's view models.
//!
//! The models in [`crate::view`] were written as "fill an input struct, call
//! once" projections with no IO of their own, and for a long time nothing
//! filled them in — the panel they describe existed only as a pure function
//! nobody called. This module is the consumer they were waiting for: it
//! reads the settings chain and the machine probe the executor and `/doctor`
//! already read, hands the result to [`crate::view::config_view`],
//! [`crate::view::overrides`] and [`crate::view::violation_view`], and turns
//! what comes back into text rows.
//!
//! It lives with the sandbox plugin rather than in the terminal for the same
//! reason `/memory`'s browser lives with the memory plugin: what it shows is
//! this plugin's to know, and switching `plugins.sandbox.enabled` off should
//! take the panel away with the confinement it describes. Registering it on
//! the `ui-registry` seat is what buys that.
//!
//! No ratatui here. The view is a [`ViewSpec::Outline`] — ordered rows of
//! text plus a footer — and the terminal draws it.

use std::path::{Path, PathBuf};

use rebon_dialog::model::{
    DialogKey, DialogModel, DialogOutcome, KeyPress, OutlineRow, OutlineView, ViewSpec,
};
use rebon_ui_seat::ids;

use crate::runtime::SandboxSettings;
use crate::view::config_view::{
    build_config_view, ConfigSection, ConfigView, ConfigViewInputs, GLOB_WARNING_PREAMBLE,
    GLOB_WARNING_TITLE,
};
use crate::view::dependency::SandboxDependencyCheck;
use crate::view::fs_config::{FsReadConfig, FsWriteConfig, NetworkConfig};
use crate::view::overrides::{
    build_overrides_view, handle_overrides_input, OverrideMode, OverridesEffect, OverridesInput,
    OverridesInputs, OverridesView, MESSAGE_LOCKED, MESSAGE_NOT_ENABLED, OVERRIDES_DOCS_URL,
};
use crate::view::platform::SandboxPlatform;
use crate::view::violation_view::{build_violation_view, ViolationView, ViolationViewInputs};

/// Stable id, shared with the front end that routes to it.
pub const DIALOG_ID: &str = ids::dialog::SANDBOX;

const TITLE: &str = " Sandbox ";
const FOOTER: &str = "Tab/Left/Right switch tab · Up/Down select · Enter apply · Esc close";

/// What a written override actually changes, and when.
///
/// [`crate::exec::SandboxPolicy`] is compiled once when a session starts, so
/// the session the user is sitting in keeps the mode it was built with. Not
/// saying so would leave them to conclude the setting did nothing.
const APPLIES_NEXT_SESSION: &str = "Takes effect in the next session.";

/// No settings source can lock the override today.
///
/// The view model has a `Locked` branch for a workspace whose sandbox
/// settings are pinned by a higher-priority configuration — a managed-policy
/// mechanism Rebon does not have. It stays wired to `false` rather than
/// deleted so the branch is here the day a policy layer lands.
const LOCKED_BY_POLICY: bool = false;

/// Nor is there a managed-domain list.
///
/// Same shape as [`LOCKED_BY_POLICY`]: the config view can title the network
/// section `(Managed)`, and nothing in the settings file says a workspace's
/// domains are managed.
const MANAGED_DOMAINS_ONLY: bool = false;

/// Stands in for the unix-socket list when `allowAllUnixSockets` is set.
///
/// The settings key is a blanket permission with no paths to list, and a tab
/// that showed an empty section for it would read as "no sockets allowed" —
/// the opposite of what it means.
const ALL_UNIX_SOCKETS: &str = "(all, allowAllUnixSockets is set)";

/// The three tabs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Config,
    Overrides,
    Violations,
}

impl Tab {
    const ORDER: [Tab; 3] = [Tab::Config, Tab::Overrides, Tab::Violations];

    fn label(self) -> &'static str {
        match self {
            Tab::Config => "Config",
            Tab::Overrides => "Overrides",
            Tab::Violations => "Violations",
        }
    }

    fn step(self, forward: bool) -> Tab {
        let index = Self::ORDER.iter().position(|tab| *tab == self).unwrap_or(0);
        let len = Self::ORDER.len();
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        Self::ORDER[next]
    }
}

/// Everything the three tabs read, gathered in one pass.
///
/// Read once when the panel opens rather than on every frame: the machine
/// probe shells out to look for `rg`, `bwrap` and `socat`, and doing that per
/// keystroke would make tab switching cost a process spawn.
#[derive(Debug, Clone)]
struct Inputs {
    settings: SandboxSettings,
    platform: SandboxPlatform,
    dep_check: SandboxDependencyCheck,
    /// Whatever went wrong while reading the settings chain, as rows.
    notes: Vec<String>,
}

fn gather(cwd: &Path) -> Inputs {
    let config_dir = rebon_config::config_home_dir();
    let platform = crate::doctor::current_sandbox_platform();
    let (settings, notes) = match crate::runtime::session::load_settings(&config_dir, cwd) {
        Ok((settings, notes)) => (settings, notes),
        // A malformed `sandbox` block is exactly what the panel should be
        // able to show: the session refuses to attach a sandbox over it, and
        // the user needs to be told which file to fix rather than shown a
        // tab full of defaults that are not what they wrote.
        Err(error) => (
            SandboxSettings::default(),
            vec![format!("sandbox settings could not be read: {error}")],
        ),
    };
    // The same probe `/doctor` runs, in the same mode and for the same
    // reason: strict is what refuses `REBON_SANDBOX_WIN_PATH`, so the report
    // names the helper the executor would use rather than one an environment
    // variable points at. Two screens deriving this differently would
    // eventually disagree, and the user believes both.
    let report = crate::runtime::session::probe_machine(
        crate::runtime::SandboxMode::Strict,
        settings.enabled,
    );
    Inputs {
        settings,
        platform,
        dep_check: report.dependency_check(),
        notes,
    }
}

/// The dependency errors, each with what to install.
///
/// `/doctor`'s remediation line is the literal "Run /sandbox for install
/// instructions", so this is the screen that has to carry them. The config
/// view model renders `warnings` and deliberately not `errors` — it is a
/// projection of the configuration, and a missing `bwrap` is a fact about
/// the machine — so the errors are appended here instead.
fn dependency_rows(inputs: &Inputs, rows: &mut Vec<OutlineRow>) {
    if inputs.dep_check.errors.is_empty() {
        return;
    }
    let classification = crate::view::dependency::classify_errors(&inputs.dep_check);
    rows.push(OutlineRow::normal(""));
    rows.push(OutlineRow::normal("Missing dependencies:"));
    for error in &inputs.dep_check.errors {
        rows.push(OutlineRow::dim(format!("  {error}")));
    }
    let mut hints = Vec::new();
    if classification.ripgrep_missing {
        hints.push(crate::view::dependency::ripgrep_install_hint(
            inputs.platform,
        ));
    }
    if classification.bwrap_missing {
        hints.push(crate::view::dependency::BWRAP_INSTALL_HINT);
    }
    if classification.socat_missing {
        hints.push(crate::view::dependency::SOCAT_INSTALL_HINT);
    }
    for hint in hints {
        rows.push(OutlineRow::normal(format!("  install: {hint}")));
    }
}

fn strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

fn config_inputs(inputs: &Inputs) -> ConfigViewInputs {
    let filesystem = &inputs.settings.session.filesystem;
    let network = &inputs.settings.session.network;
    let unix_sockets = if network.allow_all_unix_sockets {
        vec![ALL_UNIX_SOCKETS.to_string()]
    } else {
        strings(&network.allow_unix_sockets)
    };
    ConfigViewInputs {
        sandboxing_enabled: inputs.settings.enabled,
        dep_check: inputs.dep_check.clone(),
        fs_read: FsReadConfig {
            deny_only: strings(&filesystem.deny_read),
            allow_within_deny: Some(strings(&filesystem.allow_read)),
        },
        fs_write: FsWriteConfig {
            allow_only: strings(&filesystem.allow_write),
            deny_within_allow: strings(&filesystem.deny_write),
        },
        network: NetworkConfig {
            allowed_hosts: Some(network.allowed_domains.clone()),
            denied_hosts: Some(network.denied_domains.clone()),
        },
        allow_unix_sockets: Some(unix_sockets),
        excluded_commands: inputs.settings.excluded_commands.clone(),
        glob_pattern_warnings: glob_warnings(inputs),
        managed_domains_only: MANAGED_DOMAINS_ONLY,
    }
}

/// Write roots that Linux will drop.
///
/// `bwrap` mounts concrete paths, so a glob write root is refused rather
/// than silently narrowed — [`crate::runtime::config::concrete_write_roots`]
/// is where that split happens for real. Linux only: the section title says
/// so, and on macOS the seatbelt profile takes the pattern as written.
fn glob_warnings(inputs: &Inputs) -> Vec<String> {
    if !inputs.platform.is_linux() {
        return Vec::new();
    }
    inputs
        .settings
        .session
        .filesystem
        .allow_write
        .iter()
        .filter(|path| crate::runtime::config::is_glob(path))
        .map(|path| path.display().to_string())
        .collect()
}

fn overrides_inputs(settings: &SandboxSettings) -> OverridesInputs {
    OverridesInputs {
        sandboxing_enabled: settings.enabled,
        locked_by_policy: LOCKED_BY_POLICY,
        current_allow_unsandboxed: settings.allow_unsandboxed_commands,
    }
}

fn violation_inputs(inputs: &Inputs) -> ViolationViewInputs {
    let (total_count, all_violations) = crate::violations::snapshot();
    ViolationViewInputs {
        sandboxing_enabled: inputs.settings.enabled,
        platform: inputs.platform,
        total_count,
        all_violations,
    }
}

/// The panel's whole state.
#[derive(Debug, Clone)]
pub struct SandboxPanelState {
    cwd: PathBuf,
    tab: Tab,
    inputs: Inputs,
    config: ConfigView,
    overrides: OverridesView,
    /// Which of the two override modes is highlighted. Meaningless on the
    /// other two tabs, and kept across a tab switch so coming back lands
    /// where the user left.
    option: usize,
    /// What the last write said, shown under the picker.
    result: Option<String>,
}

impl SandboxPanelState {
    /// Open the panel for one workspace.
    pub fn open(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let inputs = gather(&cwd);
        let config = build_config_view(&config_inputs(&inputs));
        let overrides = build_overrides_view(&overrides_inputs(&inputs.settings));
        let option = current_option(&overrides);
        Self {
            cwd,
            tab: Tab::Config,
            inputs,
            config,
            overrides,
            option,
            result: None,
        }
    }

    #[cfg(test)]
    fn from_parts(inputs: Inputs) -> Self {
        let config = build_config_view(&config_inputs(&inputs));
        let overrides = build_overrides_view(&overrides_inputs(&inputs.settings));
        let option = current_option(&overrides);
        Self {
            cwd: PathBuf::from("."),
            tab: Tab::Config,
            inputs,
            config,
            overrides,
            option,
            result: None,
        }
    }

    /// The header, then the tab's own rows. Returns the row the selection
    /// sits on, which only the overrides picker has.
    fn body(&self) -> (Vec<OutlineRow>, Option<usize>) {
        let mut rows = vec![
            OutlineRow::normal(self.tab_header()),
            OutlineRow::normal(""),
        ];
        let mut selected = None;
        match self.tab {
            Tab::Config => {
                config_rows(&self.config, &mut rows);
                dependency_rows(&self.inputs, &mut rows);
            }
            Tab::Overrides => selected = self.overrides_rows(&mut rows),
            Tab::Violations => self.violation_rows(&mut rows),
        }
        for note in &self.inputs.notes {
            rows.push(OutlineRow::dim(format!("· {note}")));
        }
        (rows, selected)
    }

    fn tab_header(&self) -> String {
        Tab::ORDER
            .iter()
            .map(|tab| {
                if *tab == self.tab {
                    format!("[{}]", tab.label())
                } else {
                    format!(" {} ", tab.label())
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn overrides_rows(&self, rows: &mut Vec<OutlineRow>) -> Option<usize> {
        let mut selected = None;
        match &self.overrides {
            OverridesView::NotEnabled => rows.push(OutlineRow::normal(MESSAGE_NOT_ENABLED)),
            OverridesView::Locked { current } => {
                rows.push(OutlineRow::normal(MESSAGE_LOCKED));
                rows.push(OutlineRow::dim(format!(
                    "Current: {}",
                    current.base_label()
                )));
            }
            OverridesView::Interactive { options, .. } => {
                for (index, option) in options.iter().enumerate() {
                    if index == self.option {
                        selected = Some(rows.len());
                    }
                    let marker = if index == self.option { ">" } else { " " };
                    rows.push(OutlineRow::normal(format!("{marker} {}", option.label)));
                }
                rows.push(OutlineRow::normal(""));
                rows.push(OutlineRow::dim(APPLIES_NEXT_SESSION));
                rows.push(OutlineRow::dim(OVERRIDES_DOCS_URL));
            }
        }
        if let Some(result) = &self.result {
            rows.push(OutlineRow::normal(""));
            rows.push(OutlineRow::normal(result.clone()));
        }
        selected
    }

    fn violation_rows(&self, rows: &mut Vec<OutlineRow>) {
        match build_violation_view(&violation_inputs(&self.inputs)) {
            Some(view) => push_violation_view(&view, rows),
            None => rows.push(OutlineRow::dim(self.no_violations_reason())),
        }
    }

    /// Why the violations tab is empty, in the order the view model decides
    /// it: disabled first, then platform, then "nothing has been blocked".
    fn no_violations_reason(&self) -> &'static str {
        if !self.inputs.settings.enabled {
            "Sandbox is not enabled, so nothing is being blocked."
        } else if self.inputs.platform.is_linux() {
            "Blocked operations are reported by macOS only; bubblewrap fails the command instead."
        } else if !self.inputs.platform.is_mac() {
            "Blocked operations are reported by macOS only."
        } else {
            "No operations have been blocked in this process."
        }
    }

    /// Apply the highlighted override: write it, then rebuild the picker
    /// from what is now on disk.
    fn apply_override(&mut self) {
        let OverridesView::Interactive { options, .. } = &self.overrides else {
            return;
        };
        let Some(mode) = options
            .get(self.option)
            .and_then(|option| OverrideMode::from_wire(option.value))
        else {
            return;
        };
        match handle_overrides_input(OverridesInput::Select(mode)) {
            OverridesEffect::WritePolicy {
                allow_unsandboxed_commands,
                message,
            } => {
                self.result = Some(match write_override(allow_unsandboxed_commands) {
                    Ok(()) => format!("{message} {APPLIES_NEXT_SESSION}"),
                    Err(error) => format!("Could not save the setting: {error}"),
                });
            }
            // Only `Select` can reach here; `Cancel` is Esc, which the host
            // turns into a close before the reducer sees it.
            OverridesEffect::CancelSkip => {}
        }
        self.reload_overrides();
    }

    /// Re-read the settings chain so the picker shows what is on disk.
    ///
    /// Only the settings, not the machine probe: nothing about `rg` or
    /// `bwrap` changed because a boolean was written, and re-probing would
    /// spawn processes on a keypress.
    fn reload_overrides(&mut self) {
        if let Ok((settings, notes)) =
            crate::runtime::session::load_settings(&rebon_config::config_home_dir(), &self.cwd)
        {
            self.inputs.settings = settings;
            self.inputs.notes = notes;
        }
        self.overrides = build_overrides_view(&overrides_inputs(&self.inputs.settings));
        self.option = current_option(&self.overrides);
        self.config = build_config_view(&config_inputs(&self.inputs));
    }

    #[cfg(test)]
    fn tab(&self) -> Tab {
        self.tab
    }

    #[cfg(test)]
    fn option(&self) -> usize {
        self.option
    }
}

/// The row index of the mode that is currently in force, so opening the
/// picker highlights what is already set rather than always the first row.
fn current_option(overrides: &OverridesView) -> usize {
    match overrides {
        OverridesView::Interactive { options, .. } => options
            .iter()
            .position(|option| option.is_current)
            .unwrap_or(0),
        _ => 0,
    }
}

/// Persist `sandbox.allowUnsandboxedCommands` in the user layer of the
/// settings chain — the file `/kernel disable` and the settings panel write,
/// which in cowork mode is `cowork_settings.json`.
fn write_override(allow_unsandboxed_commands: bool) -> anyhow::Result<()> {
    rebon_config::save_sandbox_allow_unsandboxed_commands_in_dir(
        &rebon_config::config_home_dir(),
        allow_unsandboxed_commands,
    )
}

fn config_rows(config: &ConfigView, rows: &mut Vec<OutlineRow>) {
    if let Some(message) = config.disabled_message {
        rows.push(OutlineRow::normal(message));
        rows.push(OutlineRow::normal(""));
    }
    for section in &config.sections {
        push_section(section, rows);
    }
}

fn push_section(section: &ConfigSection, rows: &mut Vec<OutlineRow>) {
    match section {
        ConfigSection::ExcludedCommands { value } => {
            rows.push(OutlineRow::normal(format!(
                "{} {value}",
                crate::view::config_view::EXCLUDED_COMMANDS_TITLE
            )));
        }
        ConfigSection::FsRead { denied, re_allowed } => {
            rows.push(OutlineRow::normal(crate::view::config_view::FS_READ_TITLE));
            rows.push(OutlineRow::dim(format!("  Denied: {denied}")));
            if let Some(re_allowed) = re_allowed {
                rows.push(OutlineRow::dim(format!(
                    "  Allowed within denied: {re_allowed}"
                )));
            }
        }
        ConfigSection::FsWrite { allowed, excluded } => {
            rows.push(OutlineRow::normal(crate::view::config_view::FS_WRITE_TITLE));
            rows.push(OutlineRow::dim(format!("  Allowed: {allowed}")));
            if let Some(excluded) = excluded {
                rows.push(OutlineRow::dim(format!(
                    "  Excluded within allowed: {excluded}"
                )));
            }
        }
        ConfigSection::Network {
            title,
            allowed,
            denied,
        } => {
            rows.push(OutlineRow::normal(*title));
            if let Some(allowed) = allowed {
                rows.push(OutlineRow::dim(format!("  Allowed hosts: {allowed}")));
            }
            if let Some(denied) = denied {
                rows.push(OutlineRow::dim(format!("  Denied hosts: {denied}")));
            }
        }
        ConfigSection::UnixSockets { value } => {
            rows.push(OutlineRow::normal(format!(
                "{} {value}",
                crate::view::config_view::UNIX_SOCKETS_TITLE
            )));
        }
        ConfigSection::GlobWarnings {
            ignored_patterns_text,
        } => {
            rows.push(OutlineRow::normal(GLOB_WARNING_TITLE));
            rows.push(OutlineRow::dim(GLOB_WARNING_PREAMBLE));
            rows.push(OutlineRow::dim(format!("  {ignored_patterns_text}")));
        }
        ConfigSection::Warning(warning) => {
            rows.push(OutlineRow::dim(format!("⚠ {warning}")));
        }
    }
}

fn push_violation_view(view: &ViolationView, rows: &mut Vec<OutlineRow>) {
    rows.push(OutlineRow::normal(view.header.clone()));
    for row in &view.rows {
        rows.push(OutlineRow::dim(format!("  {row}")));
    }
    rows.push(OutlineRow::dim(view.footer.clone()));
}

impl DialogModel for SandboxPanelState {
    rebon_dialog::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        match press.key {
            DialogKey::Escape => return DialogOutcome::Close,
            DialogKey::Tab | DialogKey::Right => self.tab = self.tab.step(true),
            DialogKey::BackTab | DialogKey::Left => self.tab = self.tab.step(false),
            DialogKey::Up => self.option = self.option.saturating_sub(1),
            DialogKey::Down => self.option = (self.option + 1).min(option_count(&self.overrides)),
            DialogKey::Enter if self.tab == Tab::Overrides => self.apply_override(),
            _ => {}
        }
        DialogOutcome::None
    }

    fn view(&self) -> ViewSpec {
        let (rows, selected) = self.body();
        ViewSpec::Outline(OutlineView {
            title: TITLE.into(),
            rows,
            selected,
            // Selection-driven: the host scrolls the least it can to keep the
            // highlighted option on screen.
            scroll: None,
            footer: FOOTER.into(),
            empty_text: None,
        })
    }
}

/// The highest option index, so Down clamps instead of running off the list.
fn option_count(overrides: &OverridesView) -> usize {
    match overrides {
        OverridesView::Interactive { options, .. } => options.len().saturating_sub(1),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::config::{FilesystemConfig, NetworkConfig as RuntimeNetwork};

    fn inputs(settings: SandboxSettings, platform: SandboxPlatform) -> Inputs {
        Inputs {
            settings,
            platform,
            dep_check: SandboxDependencyCheck::default(),
            notes: Vec::new(),
        }
    }

    fn enabled() -> SandboxSettings {
        SandboxSettings {
            enabled: true,
            excluded_commands: vec!["git".into()],
            ..SandboxSettings::default()
        }
    }

    fn panel(settings: SandboxSettings) -> SandboxPanelState {
        SandboxPanelState::from_parts(inputs(settings, SandboxPlatform::Macos))
    }

    fn rows(panel: &SandboxPanelState) -> Vec<String> {
        panel.body().0.into_iter().map(|row| row.text).collect()
    }

    #[test]
    fn tab_stepping_wraps_in_both_directions() {
        let mut panel = panel(enabled());
        assert_eq!(panel.tab(), Tab::Config);
        panel.on_key(DialogKey::Tab.into());
        assert_eq!(panel.tab(), Tab::Overrides);
        panel.on_key(DialogKey::Right.into());
        assert_eq!(panel.tab(), Tab::Violations);
        panel.on_key(DialogKey::Tab.into());
        assert_eq!(panel.tab(), Tab::Config, "the last tab wraps to the first");
        panel.on_key(DialogKey::Left.into());
        assert_eq!(panel.tab(), Tab::Violations, "and back the other way");
    }

    #[test]
    fn escape_closes_from_any_tab() {
        let mut panel = panel(enabled());
        panel.on_key(DialogKey::Tab.into());
        assert_eq!(panel.on_key(DialogKey::Escape.into()), DialogOutcome::Close);
    }

    #[test]
    fn the_header_marks_the_open_tab() {
        let mut panel = panel(enabled());
        assert!(rows(&panel)[0].contains("[Config]"), "{:?}", rows(&panel));
        panel.on_key(DialogKey::Tab.into());
        assert!(rows(&panel)[0].contains("[Overrides]"));
        assert!(rows(&panel)[0].contains(" Config "));
    }

    #[test]
    fn the_config_tab_lists_the_sections_the_view_model_produced() {
        let mut settings = enabled();
        settings.session.filesystem = FilesystemConfig {
            allow_write: vec![PathBuf::from("/work")],
            deny_read: vec![PathBuf::from("/secrets")],
            ..FilesystemConfig::default()
        };
        settings.session.network = RuntimeNetwork {
            denied_domains: vec!["evil.example".into()],
            ..RuntimeNetwork::default()
        };
        let body = rows(&panel(settings));
        assert!(body.iter().any(|row| row == "Excluded Commands: git"));
        assert!(body.iter().any(|row| row.contains("/secrets")));
        assert!(body.iter().any(|row| row.contains("/work")));
        assert!(body.iter().any(|row| row.contains("evil.example")));
    }

    /// A blanket socket permission has no paths to list, and an empty
    /// section would read as the opposite of what it grants.
    #[test]
    fn allow_all_unix_sockets_says_so_instead_of_showing_nothing() {
        let mut settings = enabled();
        settings.session.network = RuntimeNetwork {
            allow_all_unix_sockets: true,
            ..RuntimeNetwork::default()
        };
        assert!(rows(&panel(settings))
            .iter()
            .any(|row| row.contains(ALL_UNIX_SOCKETS)));
    }

    #[test]
    fn glob_write_roots_only_warn_on_linux() {
        let mut settings = enabled();
        settings.session.filesystem = FilesystemConfig {
            allow_write: vec![PathBuf::from("/work/*/build")],
            ..FilesystemConfig::default()
        };
        let mac = inputs(settings.clone(), SandboxPlatform::Macos);
        assert!(glob_warnings(&mac).is_empty());
        let linux = inputs(settings, SandboxPlatform::Linux);
        assert_eq!(glob_warnings(&linux), vec!["/work/*/build".to_string()]);
    }

    #[test]
    fn a_disabled_sandbox_short_circuits_both_readable_tabs() {
        let mut panel = panel(SandboxSettings::default());
        let config = rows(&panel);
        assert!(config
            .iter()
            .any(|row| row == crate::view::config_view::SANDBOX_NOT_ENABLED_MESSAGE));
        panel.on_key(DialogKey::Tab.into());
        assert!(rows(&panel).iter().any(|row| row == MESSAGE_NOT_ENABLED));
        panel.on_key(DialogKey::Tab.into());
        assert!(rows(&panel)
            .iter()
            .any(|row| row.contains("Sandbox is not enabled")));
    }

    /// The picker opens on the mode that is in force, not on the first row.
    #[test]
    fn the_open_mode_is_preselected_when_it_is_the_current_one() {
        let mut settings = enabled();
        settings.allow_unsandboxed_commands = true;
        let open = panel(settings);
        assert_eq!(open.option(), 0, "`open` is the first of the two options");

        let closed = panel(enabled());
        assert_eq!(closed.option(), 1, "`closed` is the second");
    }

    /// `/doctor` sends the user here for install instructions, so this is
    /// the screen that has to carry them.
    #[test]
    fn a_missing_dependency_brings_its_install_line_with_it() {
        let mut parts = inputs(enabled(), SandboxPlatform::Linux);
        parts.dep_check = SandboxDependencyCheck {
            errors: vec!["bwrap was not found on PATH".into()],
            warnings: Vec::new(),
        };
        let body = rows(&SandboxPanelState::from_parts(parts));
        assert!(body.iter().any(|row| row == "Missing dependencies:"));
        assert!(body
            .iter()
            .any(|row| row.contains(crate::view::dependency::BWRAP_INSTALL_HINT)));
    }

    #[test]
    fn the_selection_clamps_to_the_two_modes() {
        let mut panel = panel(enabled());
        panel.on_key(DialogKey::Tab.into());
        panel.on_key(DialogKey::Down.into());
        panel.on_key(DialogKey::Down.into());
        assert_eq!(panel.option(), 1);
        panel.on_key(DialogKey::Up.into());
        panel.on_key(DialogKey::Up.into());
        assert_eq!(panel.option(), 0);
    }

    /// The highlighted row is the option row, not a row of prose above it.
    #[test]
    fn the_highlight_lands_on_the_highlighted_option() {
        let mut panel = panel(enabled());
        panel.on_key(DialogKey::Tab.into());
        let (rows, selected) = panel.body();
        let index = selected.expect("the picker highlights a row");
        assert!(rows[index].text.starts_with('>'), "{:?}", rows[index]);
        assert!(rows[index].text.contains("Strict sandbox mode"));
    }

    /// Neither read-only tab has a selection to move.
    #[test]
    fn the_other_two_tabs_highlight_nothing() {
        let panel = panel(enabled());
        assert_eq!(panel.body().1, None);
        let mut violations = panel.clone();
        violations.on_key(DialogKey::Tab.into());
        violations.on_key(DialogKey::Tab.into());
        assert_eq!(violations.body().1, None);
    }

    #[test]
    fn the_view_is_an_outline_with_a_footer() {
        let ViewSpec::Outline(view) = panel(enabled()).view() else {
            panic!("expected an outline view");
        };
        assert_eq!(view.title, TITLE);
        assert!(view.footer.contains("Esc close"));
        assert!(!view.rows.is_empty());
    }

    /// The reducer's write branch, without touching the settings file: the
    /// mapping from the highlighted row to the boolean is the part that
    /// could be wrong.
    #[test]
    fn each_option_maps_to_the_effect_the_reducer_returns() {
        let panel = panel(enabled());
        let OverridesView::Interactive { options, .. } = &panel.overrides else {
            panic!("an enabled, unlocked sandbox is interactive");
        };
        let open = OverrideMode::from_wire(options[0].value).unwrap();
        let closed = OverrideMode::from_wire(options[1].value).unwrap();
        assert_eq!(
            handle_overrides_input(OverridesInput::Select(open)),
            OverridesEffect::WritePolicy {
                allow_unsandboxed_commands: true,
                message: OverrideMode::Open.result_message(),
            }
        );
        assert_eq!(
            handle_overrides_input(OverridesInput::Select(closed)),
            OverridesEffect::WritePolicy {
                allow_unsandboxed_commands: false,
                message: OverrideMode::Closed.result_message(),
            }
        );
    }

    /// A settings file the loader could not read is a row, not a silent
    /// screen of defaults.
    #[test]
    fn loader_notes_reach_the_panel() {
        let mut parts = inputs(enabled(), SandboxPlatform::Macos);
        parts.notes = vec!["could not read project settings".into()];
        let panel = SandboxPanelState::from_parts(parts);
        assert!(rows(&panel)
            .iter()
            .any(|row| row.contains("could not read project settings")));
    }

    #[test]
    fn enter_does_nothing_on_the_read_only_tabs() {
        let mut panel = panel(enabled());
        assert_eq!(panel.on_key(DialogKey::Enter.into()), DialogOutcome::None);
        assert!(panel.result.is_none(), "the config tab wrote nothing");
    }
}
