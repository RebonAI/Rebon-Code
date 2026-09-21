//! Rate-limit message row: error text, dim upsell line, and auto-open behaviour.

/// Pinned tier literal used by the upsell logic.
pub const DEFAULT_CLAUDE_MAX_20X_TIER: &str = "default_claude_max_20x";

/// Parameters for picking the dim upsell line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsellParams {
    /// Whether the upsell should be shown.
    pub should_show_upsell: bool,
    /// Whether the tier is the max-20x tier.
    pub is_max_20x: bool,
    /// Whether the `/extra-usage` command is enabled.
    pub is_extra_usage_command_enabled: bool,
    /// Whether the interactive menu is about to auto-open.
    pub should_auto_open_rate_limit_options_menu: bool,
    /// True when the subscription type is `"team"` or `"enterprise"`.
    pub is_team_or_enterprise: bool,
    /// Whether the account has billing access.
    pub has_billing_access: bool,
}

/// Pure input seam for the rate-limit message projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitMessageInput {
    /// Main rate-limit error text.
    pub text: String,
    /// Subscription type string.
    pub subscription_type: String,
    /// Rate-limit tier string.
    pub rate_limit_tier: String,
    /// Whether mock limits are processed.
    pub should_process_mock_limits: bool,
    /// Whether the caller is on a subscription plan.
    pub is_subscriber: bool,
    /// Whether the interactive menu has already opened.
    pub has_opened_interactive_menu: bool,
    /// Status string from the subscription limits payload.
    pub subscription_limits_status: String,
    /// Whether the subscription limits payload carries a `resetsAt` value.
    pub subscription_limits_has_resets_at: bool,
    /// Whether the subscription limits payload is using overage.
    pub subscription_limits_is_using_overage: bool,
    /// Whether an open-rate-limit-options handler is present.
    pub has_open_rate_limit_options_handler: bool,
    /// Whether the `/extra-usage` command is enabled.
    pub extra_usage_command_enabled: bool,
    /// Whether the account has billing access.
    pub has_billing_access: bool,
}

/// Project a rate-limit message into its display form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitMessageProjection {
    /// Error text rendered in `"error"` color.
    pub text: String,
    /// Optional dim upsell line shown under the error.
    pub upsell_message: Option<String>,
    /// Whether this projection auto-opens the rate-limit options menu.
    pub should_auto_open_rate_limit_options_menu: bool,
    /// Whether the interactive menu counts as opened after this projection.
    pub next_has_opened_interactive_menu: bool,
}

/// Pick the dim upsell line for the given parameters.
pub fn get_upsell_message(params: UpsellParams) -> Option<&'static str> {
    if !params.should_show_upsell {
        return None;
    }

    if params.is_max_20x {
        if params.is_extra_usage_command_enabled {
            return Some("/extra-usage to finish what you\u{2019}re working on.");
        }
        return Some("/login to switch to an API usage-billed account.");
    }

    if params.should_auto_open_rate_limit_options_menu {
        return Some("Opening your options\u{2026}");
    }

    if !params.is_team_or_enterprise && !params.is_extra_usage_command_enabled {
        return Some("/upgrade to increase your usage limit.");
    }

    if params.is_team_or_enterprise {
        if !params.is_extra_usage_command_enabled {
            return None;
        }
        if params.has_billing_access {
            return Some("/extra-usage to finish what you\u{2019}re working on.");
        }
        return Some("/extra-usage to request more usage from your admin.");
    }

    Some("/upgrade or /extra-usage to finish what you\u{2019}re working on.")
}

/// Project a rate-limit message into its display form.
pub fn project_rate_limit_message(input: &RateLimitMessageInput) -> RateLimitMessageProjection {
    let is_team_or_enterprise =
        input.subscription_type == "team" || input.subscription_type == "enterprise";
    let is_max_20x = input.rate_limit_tier == DEFAULT_CLAUDE_MAX_20X_TIER;
    let should_show_upsell = input.should_process_mock_limits || input.is_subscriber;
    let can_see_rate_limit_options_upsell = should_show_upsell && !is_max_20x;
    let is_currently_rate_limited = input.subscription_limits_status == "rejected"
        && input.subscription_limits_has_resets_at
        && !input.subscription_limits_is_using_overage;
    let should_auto_open_rate_limit_options_menu = can_see_rate_limit_options_upsell
        && !input.has_opened_interactive_menu
        && is_currently_rate_limited
        && input.has_open_rate_limit_options_handler;

    let upsell_message = if input.has_opened_interactive_menu {
        None
    } else {
        get_upsell_message(UpsellParams {
            should_show_upsell,
            is_max_20x,
            is_extra_usage_command_enabled: input.extra_usage_command_enabled,
            should_auto_open_rate_limit_options_menu,
            is_team_or_enterprise,
            has_billing_access: input.has_billing_access,
        })
        .map(str::to_string)
    };

    RateLimitMessageProjection {
        text: input.text.clone(),
        upsell_message,
        should_auto_open_rate_limit_options_menu,
        next_has_opened_interactive_menu: input.has_opened_interactive_menu
            || should_auto_open_rate_limit_options_menu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> RateLimitMessageInput {
        RateLimitMessageInput {
            text: "You hit your limit".into(),
            subscription_type: "pro".into(),
            rate_limit_tier: "other".into(),
            should_process_mock_limits: false,
            is_subscriber: true,
            has_opened_interactive_menu: false,
            subscription_limits_status: "rejected".into(),
            subscription_limits_has_resets_at: true,
            subscription_limits_is_using_overage: false,
            has_open_rate_limit_options_handler: true,
            extra_usage_command_enabled: false,
            has_billing_access: false,
        }
    }

    #[test]
    fn upsell_message_handles_max_20x_and_login_branch() {
        assert_eq!(
            get_upsell_message(UpsellParams {
                should_show_upsell: true,
                is_max_20x: true,
                is_extra_usage_command_enabled: true,
                should_auto_open_rate_limit_options_menu: false,
                is_team_or_enterprise: false,
                has_billing_access: false,
            }),
            Some("/extra-usage to finish what you\u{2019}re working on.")
        );
        assert_eq!(
            get_upsell_message(UpsellParams {
                should_show_upsell: true,
                is_max_20x: true,
                is_extra_usage_command_enabled: false,
                should_auto_open_rate_limit_options_menu: false,
                is_team_or_enterprise: false,
                has_billing_access: false,
            }),
            Some("/login to switch to an API usage-billed account.")
        );
    }

    #[test]
    fn upsell_message_handles_auto_open_non_team_and_team_branches() {
        assert_eq!(
            get_upsell_message(UpsellParams {
                should_show_upsell: true,
                is_max_20x: false,
                is_extra_usage_command_enabled: true,
                should_auto_open_rate_limit_options_menu: true,
                is_team_or_enterprise: false,
                has_billing_access: false,
            }),
            Some("Opening your options\u{2026}")
        );
        assert_eq!(
            get_upsell_message(UpsellParams {
                should_show_upsell: true,
                is_max_20x: false,
                is_extra_usage_command_enabled: false,
                should_auto_open_rate_limit_options_menu: false,
                is_team_or_enterprise: false,
                has_billing_access: false,
            }),
            Some("/upgrade to increase your usage limit.")
        );
        assert_eq!(
            get_upsell_message(UpsellParams {
                should_show_upsell: true,
                is_max_20x: false,
                is_extra_usage_command_enabled: true,
                should_auto_open_rate_limit_options_menu: false,
                is_team_or_enterprise: true,
                has_billing_access: false,
            }),
            Some("/extra-usage to request more usage from your admin.")
        );
    }

    #[test]
    fn upsell_message_returns_none_when_hidden_or_team_has_no_extra_usage() {
        assert_eq!(
            get_upsell_message(UpsellParams {
                should_show_upsell: false,
                is_max_20x: false,
                is_extra_usage_command_enabled: false,
                should_auto_open_rate_limit_options_menu: false,
                is_team_or_enterprise: false,
                has_billing_access: false,
            }),
            None
        );
        assert_eq!(
            get_upsell_message(UpsellParams {
                should_show_upsell: true,
                is_max_20x: false,
                is_extra_usage_command_enabled: false,
                should_auto_open_rate_limit_options_menu: false,
                is_team_or_enterprise: true,
                has_billing_access: false,
            }),
            None
        );
    }

    #[test]
    fn projection_auto_opens_menu_only_for_active_current_rate_limit() {
        let projection = project_rate_limit_message(&base_input());
        assert_eq!(
            projection.upsell_message.as_deref(),
            Some("Opening your options\u{2026}")
        );
        assert!(projection.should_auto_open_rate_limit_options_menu);
        assert!(projection.next_has_opened_interactive_menu);

        let mut not_limited = base_input();
        not_limited.subscription_limits_status = "allowed".into();
        let projection = project_rate_limit_message(&not_limited);
        assert!(!projection.should_auto_open_rate_limit_options_menu);
    }

    #[test]
    fn projection_hides_upsell_after_menu_has_opened() {
        let mut input = base_input();
        input.has_opened_interactive_menu = true;
        let projection = project_rate_limit_message(&input);
        assert_eq!(projection.upsell_message, None);
        assert!(projection.next_has_opened_interactive_menu);
    }

    #[test]
    fn projection_uses_default_upgrade_or_extra_usage_copy() {
        let mut input = base_input();
        input.has_open_rate_limit_options_handler = false;
        input.subscription_limits_status = "allowed_warning".into();
        input.extra_usage_command_enabled = true;
        let projection = project_rate_limit_message(&input);
        assert_eq!(
            projection.upsell_message.as_deref(),
            Some("/upgrade or /extra-usage to finish what you\u{2019}re working on.")
        );
    }
}
