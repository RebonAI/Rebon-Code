//! What each tab of the settings panel says, projected off this session.
//!
//! The panel itself is `rebon_dialog::settings_dialog`: it owns the tab,
//! the selection and any text edit, and nothing else. What a status line
//! reads, how a token count is spelled and which options are editable all
//! depend on `rebon_dialog::settings_status` and on this session's live
//! numbers, none of which a reducer can reach — so they are built here
//! and handed over as finished rows.
//!
//! Writing a changed option back is the runner's, not this module's: this
//! side only reads.

use rebon_dialog::model::{PanelRow, TextSpan};
use rebon_dialog::settings_dialog::{ConfigChoice, ConfigOptionRow, SettingsProjection};
use rebon_dialog::{
    settings_status::{
        build_primary_section, build_secondary_section, format_property_value, Property,
        PropertyValue, StatusInputs,
    },
    settings_tabs::{TabId, BUILT_IN_TABS},
};
use rebon_types::{ConfigOption, ConfigOptionType};

/// Format a token count as a compact human-readable string.
///
/// Lived in the binary's `tui::dialog_support`, which is why it reads
/// like a rendering helper. It is not one: this module is the only caller,
/// and a panel that shows "12.4k" rather than "12408" is choosing wording,
/// not drawing. Moving it here cut the last real edge from `session/` to the
/// terminal outside `startup_gates`.
fn format_token_count(count: u32) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// One frame's reading of everything the panel shows.
///
/// Gathered by the renderer, which is what can see the app state and the
/// session; turned into rows here.
#[derive(Debug, Clone)]
pub struct SettingsDialogView {
    /// The running version.
    pub version: String,
    /// This session's id.
    pub session_id: String,
    /// Its custom title, when it has one.
    pub session_title: Option<String>,
    /// The directory it is bound to.
    pub cwd: String,
    /// The active provider's name.
    pub provider_name: String,
    /// The active model's name.
    pub model_name: String,
    /// The permission mode in force.
    pub permission_mode: String,
    /// Everything editable from the Config tab.
    pub config_options: Vec<ConfigOption>,
    /// How much of the context window is in use.
    pub context_usage: rebon_api::ContextUsageSnapshot,
    /// How big that window is, or zero when it is unknown.
    pub context_window: u32,
    /// Token usage for the last turn.
    pub last_turn_usage: rebon_types::Usage,
    /// Token usage for the whole session.
    pub total_usage: rebon_types::Usage,
}

/// The tab strip's labels, in the order the panel steps through them.
pub fn tab_labels() -> Vec<String> {
    BUILT_IN_TABS
        .iter()
        .map(|tab| tab.as_wire().to_string())
        .collect()
}

/// Where a tab sits in the strip. An unregistered tab — `Gates`, which
/// only an internal build appends — reads as the first, since that is the one
/// the panel would otherwise open on nothing.
pub fn tab_index(tab: TabId) -> usize {
    BUILT_IN_TABS
        .iter()
        .position(|candidate| *candidate == tab)
        .unwrap_or(0)
}

/// The tab at `index`, or the first when the index is past the strip.
pub(crate) fn tab_at(index: usize) -> TabId {
    BUILT_IN_TABS
        .get(index)
        .copied()
        .unwrap_or(BUILT_IN_TABS[0])
}

/// Everything behind the tab at `active_tab`, ready for the panel.
pub fn projection(view: &SettingsDialogView, active_tab: usize) -> SettingsProjection {
    let tab = tab_at(active_tab);
    SettingsProjection {
        tabs: tab_labels(),
        body: if tab == TabId::Config {
            Vec::new()
        } else {
            body_rows(view, tab)
        },
        config_tab: BUILT_IN_TABS
            .iter()
            .position(|candidate| *candidate == TabId::Config),
        config: config_rows(&view.config_options),
    }
}

/// The rows for one informational tab.
fn body_rows(view: &SettingsDialogView, tab: TabId) -> Vec<PanelRow> {
    match tab {
        TabId::Status => status_rows(view),
        TabId::Usage => usage_rows(view),
        // The local TUI never registers Gates: `BUILT_IN_TABS` stops at
        // Usage, and only an internal build's dispatcher appends it. The panel
        // is still reachable by id, so it must say something rather than
        // paint an empty tab.
        TabId::Gates => vec![PanelRow::one(TextSpan::dim(
            "Gates is not available in this build.",
        ))],
        TabId::Config => Vec::new(),
    }
}

/// The Status tab: the interface note, then the session and runtime
/// property sections `rebon_dialog::settings_status` builds.
fn status_rows(view: &SettingsDialogView) -> Vec<PanelRow> {
    let inputs = status_inputs(view);
    let mut rows = vec![
        PanelRow::one(TextSpan::strong(" Interface")),
        PanelRow::spans(vec![
            TextSpan::dim("UI mode: "),
            TextSpan::normal("screen / inline"),
        ]),
        PanelRow::one(TextSpan::dim(
            "Change it in the Config tab. The selection is saved as uiMode for the next launch.",
        )),
        PanelRow::blank(),
        PanelRow::one(TextSpan::strong(" Session")),
    ];
    rows.extend(build_primary_section(&inputs).iter().map(property_row));
    rows.push(PanelRow::blank());
    rows.push(PanelRow::one(TextSpan::strong(" Runtime")));
    rows.extend(build_secondary_section(&inputs).iter().map(property_row));
    rows
}

fn status_inputs(view: &SettingsDialogView) -> StatusInputs {
    StatusInputs {
        version: view.version.clone(),
        session_id: view.session_id.clone(),
        session_custom_title: view.session_title.clone(),
        cwd: view.cwd.clone(),
        account_properties: Vec::new(),
        api_provider_properties: vec![Property::new(
            "API provider",
            PropertyValue::Text(view.provider_name.clone()),
        )],
        model_label: view.model_name.clone(),
        ide_properties: Vec::new(),
        mcp_properties: Vec::new(),
        sandbox_properties: Vec::new(),
        setting_sources_properties: vec![Property::new(
            "Permission mode",
            PropertyValue::Text(view.permission_mode.clone()),
        )],
    }
}

/// One property: an emphasised label ahead of its value, or the value
/// alone for a property that has no label.
fn property_row(property: &Property) -> PanelRow {
    let value = format_property_value(&property.value);
    match property.label.as_deref() {
        Some(label) => PanelRow::spans(vec![
            TextSpan::strong(format!("{label}:")),
            TextSpan::dim(" "),
            TextSpan::normal(value),
        ]),
        None => PanelRow::one(TextSpan::normal(value)),
    }
}

/// The Usage tab: context occupancy, then the last turn and the session
/// totals.
fn usage_rows(view: &SettingsDialogView) -> Vec<PanelRow> {
    let mut rows = vec![PanelRow::one(TextSpan::strong(" Context"))];
    rows.extend(
        context_usage_lines(view)
            .into_iter()
            .map(|line| PanelRow::one(TextSpan::normal(line))),
    );
    for (heading, usage) in [
        (" Last turn", view.last_turn_usage),
        (" Session totals", view.total_usage),
    ] {
        rows.push(PanelRow::blank());
        rows.push(PanelRow::one(TextSpan::strong(heading)));
        rows.extend(
            token_usage_lines(usage)
                .into_iter()
                .map(|line| PanelRow::one(TextSpan::normal(line))),
        );
    }
    rows
}

/// The panel's view of one config option: what it is, what it is set to,
/// and what else it could be.
fn config_rows(options: &[ConfigOption]) -> Vec<ConfigOptionRow> {
    options
        .iter()
        .map(|option| ConfigOptionRow {
            id: option.id.clone(),
            name: option.name.clone(),
            description: option.description.clone(),
            free_text: matches!(option.option_type, ConfigOptionType::Text),
            current_value: option.current_value.clone(),
            choices: option
                .options
                .iter()
                .map(|candidate| ConfigChoice {
                    value: candidate.value.clone(),
                    label: candidate_label(candidate),
                })
                .collect(),
        })
        .collect()
}

fn candidate_label(candidate: &rebon_types::ConfigOptionValue) -> String {
    if candidate.name.trim().is_empty() {
        candidate.value.clone()
    } else {
        candidate.name.clone()
    }
}

fn context_usage_lines(view: &SettingsDialogView) -> Vec<String> {
    let source = view.context_usage.source.as_str();
    if view.context_window == 0 {
        return vec![format!("Context window unavailable · Source: {source}")];
    }
    if matches!(
        view.context_usage.source,
        rebon_api::ContextUsageSource::Unknown
    ) {
        return vec![
            "Usage has not been reported yet.".to_string(),
            format!(
                "Window: {} tokens · Source: {source}",
                format_token_count(view.context_window)
            ),
        ];
    }

    let used_tokens = view.context_usage.tokens;
    let used_percentage =
        (used_tokens as f64 / view.context_window as f64 * 100.0).clamp(0.0, 100.0);
    let remaining_tokens = view.context_window.saturating_sub(used_tokens);
    vec![
        format!(
            "Used: {} / {} tokens ({used_percentage:.1}%)",
            format_token_count(used_tokens),
            format_token_count(view.context_window)
        ),
        format!(
            "Remaining: {} ({:.1}%) · Source: {source}",
            format_token_count(remaining_tokens),
            100.0 - used_percentage
        ),
    ]
}

fn token_usage_lines(usage: rebon_types::Usage) -> Vec<String> {
    if usage == rebon_types::Usage::default() {
        return vec!["No usage reported yet.".to_string()];
    }
    vec![
        format!(
            "Billed input: {} · Billed output: {}",
            format_token_count(usage.billed_input_tokens()),
            format_token_count(usage.billed_output_tokens())
        ),
        format!(
            "Cache read: {} · Cache write: {}",
            format_token_count(usage.cache_read_input_tokens),
            format_token_count(usage.cache_creation_input_tokens)
        ),
        format!(
            "Prompt cache hit: {} · miss: {}",
            format_token_count(usage.prompt_cache_hit_tokens),
            format_token_count(usage.prompt_cache_miss_tokens)
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_options() -> Vec<ConfigOption> {
        vec![
            ConfigOption {
                id: "uiMode".into(),
                name: "UI mode".into(),
                description: Some("Choose screen or inline rendering".into()),
                category: Some("ui".into()),
                option_type: ConfigOptionType::Select,
                current_value: "screen".into(),
                options: vec![
                    rebon_types::ConfigOptionValue {
                        value: "screen".into(),
                        name: "Screen".into(),
                        description: Some("Fullscreen terminal UI".into()),
                    },
                    rebon_types::ConfigOptionValue {
                        value: "inline".into(),
                        // No name: the value stands in for the label.
                        name: String::new(),
                        description: None,
                    },
                ],
            },
            ConfigOption {
                id: "title".into(),
                name: "Session title".into(),
                description: None,
                category: None,
                option_type: ConfigOptionType::Text,
                current_value: "work".into(),
                options: Vec::new(),
            },
        ]
    }

    fn sample_view() -> SettingsDialogView {
        SettingsDialogView {
            version: "test".into(),
            session_id: "session".into(),
            session_title: None,
            cwd: ".".into(),
            provider_name: "test".into(),
            model_name: "test-model".into(),
            permission_mode: "default".into(),
            config_options: sample_options(),
            context_usage: rebon_api::ContextUsageSnapshot {
                tokens: 250_000,
                source: rebon_api::ContextUsageSource::Server,
            },
            context_window: 1_000_000,
            last_turn_usage: rebon_types::Usage::default(),
            total_usage: rebon_types::Usage::default(),
        }
    }

    #[test]
    fn usage_context_reports_used_remaining_and_source() {
        let lines = context_usage_lines(&sample_view());

        assert_eq!(lines[0], "Used: 250.0k / 1.0M tokens (25.0%)");
        assert_eq!(lines[1], "Remaining: 750.0k (75.0%) · Source: server");
    }

    #[test]
    fn usage_context_distinguishes_unknown_and_unavailable() {
        let mut view = sample_view();
        view.context_usage.source = rebon_api::ContextUsageSource::Unknown;
        assert_eq!(
            context_usage_lines(&view),
            vec![
                "Usage has not been reported yet.".to_string(),
                "Window: 1.0M tokens · Source: unknown".to_string(),
            ]
        );

        view.context_window = 0;
        assert_eq!(
            context_usage_lines(&view),
            vec!["Context window unavailable · Source: unknown".to_string()]
        );
    }

    #[test]
    fn usage_sections_render_real_token_breakdown() {
        let lines = token_usage_lines(rebon_types::Usage {
            input_tokens: 1_250,
            output_tokens: 500,
            cache_read_input_tokens: 2_000,
            cache_creation_input_tokens: 300,
            prompt_cache_hit_tokens: 400,
            prompt_cache_miss_tokens: 50,
            ..Default::default()
        });

        assert_eq!(lines[0], "Billed input: 1.2k · Billed output: 500");
        assert_eq!(lines[1], "Cache read: 2.0k · Cache write: 300");
        assert_eq!(lines[2], "Prompt cache hit: 400 · miss: 50");
    }

    #[test]
    fn usage_sections_prefer_provider_billed_totals() {
        let lines = token_usage_lines(rebon_types::Usage {
            total_input_tokens: 203_000,
            total_output_tokens: 4_500,
            ..Default::default()
        });

        assert_eq!(lines[0], "Billed input: 203.0k · Billed output: 4.5k");
    }

    #[test]
    fn usage_sections_report_when_no_usage_exists() {
        assert_eq!(
            token_usage_lines(rebon_types::Usage::default()),
            vec!["No usage reported yet.".to_string()]
        );
    }

    #[test]
    fn the_status_tab_leads_with_the_interface_note_and_the_session_section() {
        let rows = status_rows(&sample_view());
        let texts: Vec<String> = rows.iter().map(PanelRow::text).collect();

        assert_eq!(texts[0], " Interface");
        assert_eq!(texts[1], "UI mode: screen / inline");
        assert_eq!(texts[4], " Session");
        assert!(texts.contains(&" Runtime".to_string()), "{texts:?}");
        assert!(
            texts.iter().any(|text| text.contains("session")),
            "the session id is shown: {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.contains("test-model")),
            "the model is shown: {texts:?}"
        );
    }

    #[test]
    fn the_usage_tab_has_a_section_per_scope() {
        let texts: Vec<String> = usage_rows(&sample_view())
            .iter()
            .map(PanelRow::text)
            .collect();

        assert_eq!(texts[0], " Context");
        assert_eq!(texts[1], "Used: 250.0k / 1.0M tokens (25.0%)");
        assert!(texts.contains(&" Last turn".to_string()), "{texts:?}");
        assert!(texts.contains(&" Session totals".to_string()), "{texts:?}");
    }

    #[test]
    fn config_rows_carry_the_edit_style_and_fall_back_to_the_raw_value_as_a_label() {
        let rows = config_rows(&sample_options());

        assert!(!rows[0].free_text);
        assert_eq!(rows[0].choices[0].label, "Screen");
        assert_eq!(rows[0].choices[1].label, "inline");
        assert!(rows[1].free_text);
        assert!(rows[1].choices.is_empty());
    }

    #[test]
    fn the_projection_builds_rows_for_the_active_tab_only() {
        let view = sample_view();
        let config_tab = tab_index(TabId::Config);

        let status = projection(&view, tab_index(TabId::Status));
        assert_eq!(status.config_tab, Some(config_tab));
        assert_eq!(status.tabs, tab_labels());
        assert!(!status.body.is_empty());
        // The options travel on every tab: the panel clamps against them
        // whether or not their tab is the one on screen.
        assert_eq!(status.config.len(), 2);

        // The config tab's rows are the panel's to build, not ours.
        assert!(projection(&view, config_tab).body.is_empty());
    }

    #[test]
    fn a_tab_index_round_trips_and_an_unregistered_tab_reads_as_the_first() {
        for (index, tab) in BUILT_IN_TABS.iter().enumerate() {
            assert_eq!(tab_index(*tab), index);
            assert_eq!(tab_at(index), *tab);
        }
        assert_eq!(tab_index(TabId::Gates), 0, "Gates is not in this build");
        assert_eq!(tab_at(BUILT_IN_TABS.len()), BUILT_IN_TABS[0]);
    }

    #[test]
    fn the_gates_tab_says_it_is_unavailable_rather_than_painting_nothing() {
        let rows = body_rows(&sample_view(), TabId::Gates);
        assert_eq!(rows[0].text(), "Gates is not available in this build.");
    }
    #[test]
    fn format_token_count_uses_compact_suffixes() {
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_250), "1.2k");
        assert_eq!(format_token_count(1_250_000), "1.2M");
    }
}
