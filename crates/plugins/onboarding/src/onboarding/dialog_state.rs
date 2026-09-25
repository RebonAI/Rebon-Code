//! The onboarding dialog's own state vocabulary: which step the wizard
//! is on, which pane the setup step shows, which form field has focus,
//! and how the login pane's rows are grouped.
//!
//! These are the parts of the onboarding dialog that answer a
//! question with no terminal in it. The dialog that draws them keeps the
//! rendering and the on-disk side effects; what lives here is decidable
//! from its arguments alone.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Theme,
    UiMode,
    Provider,
    Migration,
    Security,
    Done,
}

impl Step {
    pub fn label(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::UiMode => "ui mode",
            Self::Provider => "setup",
            Self::Migration => "import",
            Self::Security => "security",
            Self::Done => "ready",
        }
    }
}

/// Which pane is visible inside the Provider step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupPane {
    /// Existing onboarding/provider state must be preserved or explicitly reopened.
    ExistingSetup,
    /// Built-in login methods + "Add custom provider" CTA.
    LoginMethods,
    /// Form for adding or editing a custom provider + models list for
    /// the currently-selected provider.
    ProviderPanel,
}

/// Which mode the provider form is currently in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderFormMode {
    Add,
    Edit { original_name: String },
}

impl ProviderFormMode {
    pub fn original_name(&self) -> Option<&str> {
        match self {
            Self::Add => None,
            Self::Edit { original_name } => Some(original_name),
        }
    }
}

/// Which field in the provider form currently has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderField {
    ProviderList,
    Preset,
    Name,
    ApiKey,
    BaseUrl,
    Format,
    Model,
    SaveProviderButton,
    NewModelInput,
    AddModelButton,
}

impl ProviderField {
    pub const FORM_ORDER: [Self; 6] = [
        Self::Name,
        Self::ApiKey,
        Self::BaseUrl,
        Self::Format,
        Self::Model,
        Self::SaveProviderButton,
    ];

    pub const ADD_FORM_ORDER: [Self; 7] = [
        Self::Preset,
        Self::Name,
        Self::ApiKey,
        Self::BaseUrl,
        Self::Format,
        Self::Model,
        Self::SaveProviderButton,
    ];

    pub const MODELS_ORDER: [Self; 2] = [Self::NewModelInput, Self::AddModelButton];

    pub fn label(self) -> &'static str {
        match self {
            Self::ProviderList => "Providers",
            Self::Preset => "Quick setup",
            Self::Name => "Provider name",
            Self::ApiKey => "API key",
            Self::BaseUrl => "Base URL",
            Self::Format => "API protocol",
            Self::Model => "Initial model",
            Self::SaveProviderButton => "[ Save Provider ]",
            Self::NewModelInput => "New model",
            Self::AddModelButton => "[ Add Model ]",
        }
    }
}

/// How many quick-setup presets the add form shows at once.
pub const PRESET_ROWS_VISIBLE: usize = 6;

/// The `[start, end)` slice of a list of `total` rows that keeps `selected`
/// visible inside a window of [`PRESET_ROWS_VISIBLE`] rows.
pub fn preset_window(total: usize, selected: usize) -> (usize, usize) {
    if total <= PRESET_ROWS_VISIBLE {
        return (0, total);
    }
    let selected = selected.min(total.saturating_sub(1));
    let start = selected
        .saturating_sub(PRESET_ROWS_VISIBLE / 2)
        .min(total - PRESET_ROWS_VISIBLE);
    (start, start + PRESET_ROWS_VISIBLE)
}

/// One option in the login-method picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginMethodOption {
    pub value: &'static str,
    pub primary_label: &'static str,
    pub secondary_label: &'static str,
}

/// The login methods the picker offers, in display order: one row per
/// account login in `rebon_config::account_login`'s table.
pub fn actionable_login_options() -> Vec<LoginMethodOption> {
    rebon_config::account_logins()
        .iter()
        .map(|spec| LoginMethodOption {
            value: spec.id,
            primary_label: spec.picker_label,
            secondary_label: spec.description,
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginPaneRow {
    Header(&'static str),
    Option(usize),
    CustomProviderCta,
}

/// The heading a login row sits under: its login's group, or "Custom" for
/// anything the table does not know.
pub fn login_option_group(value: &str) -> &'static str {
    rebon_config::account_logins()
        .iter()
        .find(|spec| spec.id == value)
        .map(|spec| spec.group)
        .unwrap_or("Custom")
}

pub fn grouped_login_pane_rows(options: &[LoginMethodOption]) -> Vec<LoginPaneRow> {
    let mut groups: Vec<&'static str> = Vec::new();
    for spec in rebon_config::account_logins() {
        if !groups.contains(&spec.group) {
            groups.push(spec.group);
        }
    }
    groups.push("Custom");
    let mut rows = Vec::new();

    for group in groups {
        let option_indices: Vec<usize> = options
            .iter()
            .enumerate()
            .filter_map(|(idx, option)| (login_option_group(option.value) == group).then_some(idx))
            .collect();
        let has_custom_cta = group == "Custom";
        if option_indices.is_empty() && !has_custom_cta {
            continue;
        }

        if !rows.is_empty() {
            rows.push(LoginPaneRow::Header(""));
        }
        rows.push(LoginPaneRow::Header(group));
        rows.extend(option_indices.into_iter().map(LoginPaneRow::Option));
        if has_custom_cta {
            rows.push(LoginPaneRow::CustomProviderCta);
        }
    }

    rows
}

pub fn login_row_for_focus(options: &[LoginMethodOption], focus: usize) -> LoginPaneRow {
    grouped_login_pane_rows(options)
        .into_iter()
        .filter(|row| !matches!(row, LoginPaneRow::Header(_)))
        .nth(focus)
        .unwrap_or(LoginPaneRow::CustomProviderCta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_window_keeps_the_selection_visible_without_growing() {
        assert_eq!(preset_window(4, 2), (0, 4), "short lists are not windowed");
        assert_eq!(preset_window(17, 0), (0, 6));
        assert_eq!(preset_window(17, 3), (0, 6));
        assert_eq!(preset_window(17, 8), (5, 11));
        assert_eq!(preset_window(17, 16), (11, 17), "the tail stays full");
        assert_eq!(
            preset_window(17, 40),
            (11, 17),
            "an out-of-range index clamps"
        );
        for selected in 0..17 {
            let (start, end) = preset_window(17, selected);
            assert!(start <= selected && selected < end, "{selected}");
            assert_eq!(end - start, PRESET_ROWS_VISIBLE);
        }
    }

    #[test]
    fn login_pane_grouping_order_keeps_navigation_indexes_selectable_only() {
        // An option outside the OpenAI group is listed first but lands
        // under "Custom", so the row order differs from the index order.
        let mut options = vec![LoginMethodOption {
            value: "other",
            primary_label: "Other",
            secondary_label: "",
        }];
        options.extend(actionable_login_options());
        let rows = grouped_login_pane_rows(&options);
        assert_eq!(
            rows,
            vec![
                LoginPaneRow::Header("OpenAI"),
                LoginPaneRow::Option(1),
                LoginPaneRow::Header(""),
                LoginPaneRow::Header("GitHub"),
                LoginPaneRow::Option(2),
                LoginPaneRow::Header(""),
                LoginPaneRow::Header("Custom"),
                LoginPaneRow::Option(0),
                LoginPaneRow::CustomProviderCta,
            ]
        );

        assert_eq!(login_row_for_focus(&options, 0), LoginPaneRow::Option(1));
        assert_eq!(login_row_for_focus(&options, 1), LoginPaneRow::Option(2));
        assert_eq!(login_row_for_focus(&options, 2), LoginPaneRow::Option(0));
        assert_eq!(
            login_row_for_focus(&options, 3),
            LoginPaneRow::CustomProviderCta
        );
    }

    /// The picker is the account-login table, row for row, with the
    /// ChatGPT row first and worded as it always was.
    #[test]
    fn the_picker_offers_every_account_login_in_table_order() {
        let options = actionable_login_options();
        let values: Vec<&str> = options.iter().map(|option| option.value).collect();
        assert_eq!(values, vec!["openai", "copilot"]);
        assert_eq!(options[0].primary_label, "OpenAI account");
        assert!(options[0].secondary_label.contains("Codex OAuth"));
        assert_eq!(options[1].primary_label, "GitHub Copilot account");
        assert_eq!(
            grouped_login_pane_rows(&options),
            vec![
                LoginPaneRow::Header("OpenAI"),
                LoginPaneRow::Option(0),
                LoginPaneRow::Header(""),
                LoginPaneRow::Header("GitHub"),
                LoginPaneRow::Option(1),
                LoginPaneRow::Header(""),
                LoginPaneRow::Header("Custom"),
                LoginPaneRow::CustomProviderCta,
            ]
        );
        assert_eq!(login_option_group("copilot"), "GitHub");
        assert_eq!(login_option_group("nope"), "Custom");
    }
}
