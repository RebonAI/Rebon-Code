//! View-model for the sandbox config tab. Pure projection of the
//! sandbox configuration into a list of section structs the renderer
//! can lay out without re-deriving any predicates.
//!
//! ## Behavior notes
//!
//! [`build_config_view`] — the whole
//! config tab, minus the UI rendering. The pure logic is:
//!
//! ```text
//! if !sandboxing_enabled:
//!     disabled_message = "Sandbox is not enabled"
//!     sections         = the `dep_check.warnings` rows only
//! else:
//!     sections = [
//!         ExcludedCommands { .. }  // always, even when empty
//!         FsRead   { .. }          // iff deny_only is non-empty
//!         FsWrite  { .. }          // iff allow_only is non-empty
//!         Network  { .. }          // iff allowed/denied hosts non-empty
//!         UnixSockets { .. }       // iff present and non-empty
//!         GlobWarnings { .. }      // iff non-empty, head truncated to 3
//!         Warning(_)               // one per `dep_check.warnings` entry
//!     ]
//! ```
//!
//! ## Pinned rules
//!
//! 1. **Disabled state has its own short-circuit.** When sandboxing
//! is disabled the only thing the tab renders is "Sandbox is not
//! enabled" plus the dependency-check warning rows. No other section
//! renders.
//! 2. **Excluded commands ALWAYS render.** Even when the list is
//! empty, the section renders with the literal `"None"`. Pinned
//! by [`build_excluded_commands_value`].
//! 3. **Excluded commands separator is `", "`.** Including the space.
//! Pinned by `excluded_commands_join_with_space`.
//! 4. **Each restriction section is gated on its own non-empty
//! predicate.** No empty sections render. Pinned by
//! [`ConfigSection`] variants being optional in [`build_config_view`].
//! 5. **Glob warnings show ONLY the first 3.** When there are more
//! than 3, the suffix `" (N more)"` is appended. Pinned by
//! [`format_glob_warnings`].
//! 6. **Glob warning truncation joins with `", "`.** Same as the
//! other sections.
//! 7. **Managed-domains label suffix is `" (Managed)"`.** When
//! `managed_domains_only` is true the network
//! section title is `"Network Restrictions (Managed):"` instead of
//! `"Network Restrictions:"`. Pinned by [`network_section_title`].
//! 8. **Disabled message is `"Sandbox is not enabled"`.** Unchanged.
//! 9. **Re-allow rows render only when there are paths.** A `Some([])`
//! or `None` `allow_within_deny` does NOT render the
//! `"Allowed within denied:"` row.

use crate::view::dependency::SandboxDependencyCheck;
use crate::view::fs_config::{FsReadConfig, FsWriteConfig, NetworkConfig};

/// `"None"` — placeholder when `excluded_commands` is empty. Pinned
/// literally.
pub const EXCLUDED_COMMANDS_NONE_PLACEHOLDER: &str = "None";

/// `"Sandbox is not enabled"` — the disabled-state short-circuit
/// message.
pub const SANDBOX_NOT_ENABLED_MESSAGE: &str = "Sandbox is not enabled";

/// `"Excluded Commands:"` — section title.
pub const EXCLUDED_COMMANDS_TITLE: &str = "Excluded Commands:";

/// `"Filesystem Read Restrictions:"` — section title.
pub const FS_READ_TITLE: &str = "Filesystem Read Restrictions:";

/// `"Filesystem Write Restrictions:"` — section title.
pub const FS_WRITE_TITLE: &str = "Filesystem Write Restrictions:";

/// `"Network Restrictions:"` — section title (unmanaged variant).
pub const NETWORK_TITLE_UNMANAGED: &str = "Network Restrictions:";

/// `"Network Restrictions (Managed):"` — section title (managed
/// variant). The `(Managed)` suffix appears when
/// `managed_domains_only` is true.
pub const NETWORK_TITLE_MANAGED: &str = "Network Restrictions (Managed):";

/// `"Allowed Unix Sockets:"` — section title.
pub const UNIX_SOCKETS_TITLE: &str = "Allowed Unix Sockets:";

/// Glob-warnings section title (full literal — `⚠ Warning:` is
/// U+26A0 WARNING SIGN).
pub const GLOB_WARNING_TITLE: &str = "⚠ Warning: Glob patterns not fully supported on Linux";

/// Glob-warnings preamble — `"The following patterns will be
/// ignored:"`. Note the literal trailing space-and-list join.
pub const GLOB_WARNING_PREAMBLE: &str = "The following patterns will be ignored:";

/// Inputs to the config view. The consumer fills this in from the
/// real adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigViewInputs {
    /// Whether sandboxing is currently enabled.
    pub sandboxing_enabled: bool,
    /// Dependency check result (errors + warnings).
    pub dep_check: SandboxDependencyCheck,
    /// Filesystem read restrictions.
    pub fs_read: FsReadConfig,
    /// Filesystem write restrictions.
    pub fs_write: FsWriteConfig,
    /// Network restriction configuration.
    pub network: NetworkConfig,
    /// Allowed unix sockets. `None` matches
    /// `absent`.
    pub allow_unix_sockets: Option<Vec<String>>,
    /// Excluded commands. Always present.
    pub excluded_commands: Vec<String>,
    /// Linux glob-pattern warnings. Always present.
    pub glob_pattern_warnings: Vec<String>,
    /// Whether only managed sandbox domains are allowed.
    pub managed_domains_only: bool,
}

impl Default for ConfigViewInputs {
    fn default() -> Self {
        Self {
            sandboxing_enabled: false,
            dep_check: SandboxDependencyCheck::default(),
            fs_read: FsReadConfig::default(),
            fs_write: FsWriteConfig::default(),
            network: NetworkConfig::default(),
            allow_unix_sockets: None,
            excluded_commands: Vec::new(),
            glob_pattern_warnings: Vec::new(),
            managed_domains_only: false,
        }
    }
}

/// One section row in the rendered config tab. Each variant is built by
/// [`build_config_view`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSection {
    /// "Excluded Commands: <list-or-None>".
    ExcludedCommands { value: String },
    /// "Filesystem Read Restrictions:" with denied list and (optional)
    /// re-allowed list.
    FsRead {
        denied: String,
        re_allowed: Option<String>,
    },
    /// "Filesystem Write Restrictions:" with allowed list and
    /// (optional) excluded-within-allowed list.
    FsWrite {
        allowed: String,
        excluded: Option<String>,
    },
    /// "Network Restrictions[(Managed)]:" with up to two host lists.
    Network {
        title: &'static str,
        allowed: Option<String>,
        denied: Option<String>,
    },
    /// "Allowed Unix Sockets: <list>".
    UnixSockets { value: String },
    /// Glob warnings block. Renders the truncated list with optional
    /// `(N more)` suffix.
    GlobWarnings { ignored_patterns_text: String },
    /// One row per entry of `dep_check.warnings`, in input order.
    Warning(String),
}

/// Whole rendered config view. The renderer just walks the
/// [`ConfigView::sections`] in order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigView {
    /// `Some(message)` when the tab is in the disabled-state
    /// short-circuit. Pinned to [`SANDBOX_NOT_ENABLED_MESSAGE`].
    pub disabled_message: Option<&'static str>,
    /// Ordered list of sections to render below the disabled-message.
    /// In the disabled state this contains only the warning rows.
    pub sections: Vec<ConfigSection>,
}

/// Build the `"Excluded Commands:"` value text. Returns `"None"`
/// when the list is empty, otherwise the comma-and-space-joined list.
pub fn build_excluded_commands_value(commands: &[String]) -> String {
    if commands.is_empty() {
        EXCLUDED_COMMANDS_NONE_PLACEHOLDER.to_string()
    } else {
        commands.join(", ")
    }
}

/// Pick the network section title based on whether managed domains
/// are enforced.
pub fn network_section_title(managed: bool) -> &'static str {
    if managed {
        NETWORK_TITLE_MANAGED
    } else {
        NETWORK_TITLE_UNMANAGED
    }
}

/// Format the glob warnings list with the truncate-to-3 + `(N more)`
/// suffix:
///
/// ```text
/// warnings.iter().take(3).join(", ") +
///     if warnings.len() > 3 { format!(" ({} more)", warnings.len() - 3) } else { String::new() }
/// ```
pub fn format_glob_warnings(warnings: &[String]) -> String {
    let head: Vec<String> = warnings.iter().take(3).cloned().collect();
    let mut s = head.join(", ");
    if warnings.len() > 3 {
        s.push_str(&format!(" ({} more)", warnings.len() - 3));
    }
    s
}

/// Build the rendered config view. Pure function of the inputs.
pub fn build_config_view(inputs: &ConfigViewInputs) -> ConfigView {
    let mut sections = Vec::new();

    // Warnings note (always built so we can share between branches).
    let warning_sections: Vec<ConfigSection> = inputs
        .dep_check
        .warnings
        .iter()
        .map(|w| ConfigSection::Warning(w.clone()))
        .collect();

    if !inputs.sandboxing_enabled {
        sections.extend(warning_sections);
        return ConfigView {
            disabled_message: Some(SANDBOX_NOT_ENABLED_MESSAGE),
            sections,
        };
    }

    // Excluded commands — always rendered, even when empty.
    sections.push(ConfigSection::ExcludedCommands {
        value: build_excluded_commands_value(&inputs.excluded_commands),
    });

    // Filesystem read restrictions
    if inputs.fs_read.has_restrictions() {
        sections.push(ConfigSection::FsRead {
            denied: inputs.fs_read.deny_only.join(", "),
            re_allowed: if inputs.fs_read.has_re_allowed_paths() {
                inputs
                    .fs_read
                    .allow_within_deny
                    .as_ref()
                    .map(|v| v.join(", "))
            } else {
                None
            },
        });
    }

    // Filesystem write restrictions
    if inputs.fs_write.has_restrictions() {
        sections.push(ConfigSection::FsWrite {
            allowed: inputs.fs_write.allow_only.join(", "),
            excluded: if inputs.fs_write.has_excluded_paths() {
                Some(inputs.fs_write.deny_within_allow.join(", "))
            } else {
                None
            },
        });
    }

    // Network restrictions
    if inputs.network.has_any_restriction() {
        sections.push(ConfigSection::Network {
            title: network_section_title(inputs.managed_domains_only),
            allowed: if inputs.network.has_allowed() {
                inputs.network.allowed_hosts.as_ref().map(|v| v.join(", "))
            } else {
                None
            },
            denied: if inputs.network.has_denied() {
                inputs.network.denied_hosts.as_ref().map(|v| v.join(", "))
            } else {
                None
            },
        });
    }

    // Allowed unix sockets — gated on `Some` AND non-empty.
    if let Some(sockets) = &inputs.allow_unix_sockets {
        if !sockets.is_empty() {
            sections.push(ConfigSection::UnixSockets {
                value: sockets.join(", "),
            });
        }
    }

    // Glob pattern warnings
    if !inputs.glob_pattern_warnings.is_empty() {
        sections.push(ConfigSection::GlobWarnings {
            ignored_patterns_text: format_glob_warnings(&inputs.glob_pattern_warnings),
        });
    }

    // Warnings note
    sections.extend(warning_sections);

    ConfigView {
        disabled_message: None,
        sections,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn enabled_inputs() -> ConfigViewInputs {
        ConfigViewInputs {
            sandboxing_enabled: true,
            ..ConfigViewInputs::default()
        }
    }

    #[test]
    fn excluded_value_none_when_empty() {
        assert_eq!(build_excluded_commands_value(&[]), "None");
    }

    #[test]
    fn excluded_value_single_command() {
        assert_eq!(build_excluded_commands_value(&s(&["git"])), "git");
    }

    #[test]
    fn excluded_value_multiple_commands_joined_with_comma_space() {
        assert_eq!(
            build_excluded_commands_value(&s(&["git", "ls", "cat"])),
            "git, ls, cat"
        );
    }

    #[test]
    fn excluded_commands_join_with_space() {
        // The separator is a comma followed by a SPACE. We pin the
        // literal separator.
        assert_eq!(build_excluded_commands_value(&s(&["a", "b"])), "a, b");
    }

    #[test]
    fn network_title_unmanaged() {
        assert_eq!(network_section_title(false), "Network Restrictions:");
    }

    #[test]
    fn network_title_managed() {
        assert_eq!(
            network_section_title(true),
            "Network Restrictions (Managed):"
        );
    }

    #[test]
    fn glob_warnings_empty_yields_empty() {
        assert_eq!(format_glob_warnings(&[]), "");
    }

    #[test]
    fn glob_warnings_one_no_more_suffix() {
        assert_eq!(format_glob_warnings(&s(&["**/*.rs"])), "**/*.rs");
    }

    #[test]
    fn glob_warnings_three_no_more_suffix() {
        assert_eq!(format_glob_warnings(&s(&["a", "b", "c"])), "a, b, c");
    }

    #[test]
    fn glob_warnings_four_truncates_with_one_more() {
        assert_eq!(
            format_glob_warnings(&s(&["a", "b", "c", "d"])),
            "a, b, c (1 more)"
        );
    }

    #[test]
    fn glob_warnings_ten_truncates_with_seven_more() {
        let v = s(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        assert_eq!(format_glob_warnings(&v), "a, b, c (7 more)");
    }

    #[test]
    fn build_view_disabled_short_circuits() {
        let inputs = ConfigViewInputs::default();
        let v = build_config_view(&inputs);
        assert_eq!(v.disabled_message, Some(SANDBOX_NOT_ENABLED_MESSAGE));
        assert!(v.sections.is_empty());
    }

    #[test]
    fn build_view_disabled_includes_warnings() {
        let mut inputs = ConfigViewInputs::default();
        inputs.dep_check.warnings = s(&["seccomp missing"]);
        let v = build_config_view(&inputs);
        assert_eq!(v.disabled_message, Some(SANDBOX_NOT_ENABLED_MESSAGE));
        assert_eq!(
            v.sections,
            vec![ConfigSection::Warning("seccomp missing".to_string())]
        );
    }

    #[test]
    fn build_view_disabled_does_not_render_other_sections() {
        // Even when fs configs are populated, disabled state hides
        // them.
        let mut inputs = ConfigViewInputs::default();
        inputs.fs_read.deny_only = s(&["/etc"]);
        inputs.excluded_commands = s(&["git"]);
        let v = build_config_view(&inputs);
        assert_eq!(v.disabled_message, Some(SANDBOX_NOT_ENABLED_MESSAGE));
        assert!(v.sections.is_empty());
    }

    #[test]
    fn build_view_enabled_always_has_excluded_section() {
        let inputs = enabled_inputs();
        let v = build_config_view(&inputs);
        assert!(v.disabled_message.is_none());
        assert_eq!(
            v.sections[0],
            ConfigSection::ExcludedCommands {
                value: "None".to_string()
            }
        );
    }

    #[test]
    fn build_view_enabled_excluded_with_commands() {
        let mut inputs = enabled_inputs();
        inputs.excluded_commands = s(&["git", "ls"]);
        let v = build_config_view(&inputs);
        assert_eq!(
            v.sections[0],
            ConfigSection::ExcludedCommands {
                value: "git, ls".to_string()
            }
        );
    }

    #[test]
    fn build_view_fs_read_section_only_when_denied_nonempty() {
        let mut inputs = enabled_inputs();
        // No deny → no section
        let v = build_config_view(&inputs);
        assert!(!v
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::FsRead { .. })));
        // With deny
        inputs.fs_read.deny_only = s(&["/etc"]);
        let v = build_config_view(&inputs);
        assert!(v
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::FsRead { .. })));
    }

    #[test]
    fn build_view_fs_read_re_allowed_only_when_nonempty() {
        let mut inputs = enabled_inputs();
        inputs.fs_read.deny_only = s(&["/etc"]);
        inputs.fs_read.allow_within_deny = Some(vec![]);
        let v = build_config_view(&inputs);
        let read = v
            .sections
            .iter()
            .find_map(|s| match s {
                ConfigSection::FsRead { re_allowed, .. } => Some(re_allowed.clone()),
                _ => None,
            })
            .expect("read section");
        assert_eq!(read, None);

        inputs.fs_read.allow_within_deny = Some(s(&["/etc/ssl"]));
        let v = build_config_view(&inputs);
        let read = v
            .sections
            .iter()
            .find_map(|s| match s {
                ConfigSection::FsRead { re_allowed, .. } => Some(re_allowed.clone()),
                _ => None,
            })
            .expect("read section");
        assert_eq!(read, Some("/etc/ssl".to_string()));
    }

    #[test]
    fn build_view_fs_write_section_only_when_allowed_nonempty() {
        let mut inputs = enabled_inputs();
        let v = build_config_view(&inputs);
        assert!(!v
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::FsWrite { .. })));
        inputs.fs_write.allow_only = s(&["/tmp"]);
        let v = build_config_view(&inputs);
        assert!(v
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::FsWrite { .. })));
    }

    #[test]
    fn build_view_fs_write_excluded_only_when_nonempty() {
        let mut inputs = enabled_inputs();
        inputs.fs_write.allow_only = s(&["/tmp"]);
        let v = build_config_view(&inputs);
        let excluded = v
            .sections
            .iter()
            .find_map(|s| match s {
                ConfigSection::FsWrite { excluded, .. } => Some(excluded.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(excluded, None);

        inputs.fs_write.deny_within_allow = s(&["/tmp/secrets"]);
        let v = build_config_view(&inputs);
        let excluded = v
            .sections
            .iter()
            .find_map(|s| match s {
                ConfigSection::FsWrite { excluded, .. } => Some(excluded.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(excluded, Some("/tmp/secrets".to_string()));
    }

    #[test]
    fn build_view_network_section_only_when_any_restriction() {
        let mut inputs = enabled_inputs();
        let v = build_config_view(&inputs);
        assert!(!v
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::Network { .. })));
        inputs.network.allowed_hosts = Some(s(&["api.example.com"]));
        let v = build_config_view(&inputs);
        let net = v
            .sections
            .iter()
            .find_map(|s| match s {
                ConfigSection::Network {
                    title,
                    allowed,
                    denied,
                } => Some((*title, allowed.clone(), denied.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(net.0, "Network Restrictions:");
        assert_eq!(net.1, Some("api.example.com".to_string()));
        assert_eq!(net.2, None);
    }

    #[test]
    fn build_view_network_managed_title() {
        let mut inputs = enabled_inputs();
        inputs.managed_domains_only = true;
        inputs.network.denied_hosts = Some(s(&["evil.com"]));
        let v = build_config_view(&inputs);
        let title = v
            .sections
            .iter()
            .find_map(|s| match s {
                ConfigSection::Network { title, .. } => Some(*title),
                _ => None,
            })
            .unwrap();
        assert_eq!(title, "Network Restrictions (Managed):");
    }

    #[test]
    fn build_view_unix_sockets_only_when_some_and_nonempty() {
        let mut inputs = enabled_inputs();
        // None
        assert!(!build_config_view(&inputs)
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::UnixSockets { .. })));
        // Some([])
        inputs.allow_unix_sockets = Some(vec![]);
        assert!(!build_config_view(&inputs)
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::UnixSockets { .. })));
        // Some(["/tmp/foo.sock"])
        inputs.allow_unix_sockets = Some(s(&["/tmp/foo.sock"]));
        assert!(build_config_view(&inputs)
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::UnixSockets { .. })));
    }

    #[test]
    fn build_view_glob_warnings_only_when_nonempty() {
        let mut inputs = enabled_inputs();
        let v = build_config_view(&inputs);
        assert!(!v
            .sections
            .iter()
            .any(|s| matches!(s, ConfigSection::GlobWarnings { .. })));
        inputs.glob_pattern_warnings = s(&["a", "b", "c", "d", "e"]);
        let v = build_config_view(&inputs);
        let glob = v
            .sections
            .iter()
            .find_map(|s| match s {
                ConfigSection::GlobWarnings {
                    ignored_patterns_text,
                } => Some(ignored_patterns_text.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(glob, "a, b, c (2 more)");
    }

    #[test]
    fn build_view_warnings_appended_at_end() {
        let mut inputs = enabled_inputs();
        inputs.dep_check.warnings = s(&["seccomp missing"]);
        let v = build_config_view(&inputs);
        // Last section should be the warning
        assert_eq!(
            v.sections.last().cloned(),
            Some(ConfigSection::Warning("seccomp missing".to_string()))
        );
    }

    #[test]
    fn build_view_full_render_section_order() {
        let mut inputs = enabled_inputs();
        inputs.excluded_commands = s(&["git"]);
        inputs.fs_read.deny_only = s(&["/etc"]);
        inputs.fs_write.allow_only = s(&["/tmp"]);
        inputs.network.denied_hosts = Some(s(&["evil.com"]));
        inputs.allow_unix_sockets = Some(s(&["/tmp/foo.sock"]));
        inputs.glob_pattern_warnings = s(&["a", "b"]);
        inputs.dep_check.warnings = s(&["seccomp missing"]);

        let v = build_config_view(&inputs);
        // Section order in the rendered layout: Excluded, FsRead, FsWrite,
        // Network, UnixSockets, GlobWarnings, Warnings.
        assert_eq!(v.sections.len(), 7);
        assert!(matches!(
            v.sections[0],
            ConfigSection::ExcludedCommands { .. }
        ));
        assert!(matches!(v.sections[1], ConfigSection::FsRead { .. }));
        assert!(matches!(v.sections[2], ConfigSection::FsWrite { .. }));
        assert!(matches!(v.sections[3], ConfigSection::Network { .. }));
        assert!(matches!(v.sections[4], ConfigSection::UnixSockets { .. }));
        assert!(matches!(v.sections[5], ConfigSection::GlobWarnings { .. }));
        assert!(matches!(v.sections[6], ConfigSection::Warning(_)));
    }

    #[test]
    fn pinned_constants() {
        assert_eq!(EXCLUDED_COMMANDS_NONE_PLACEHOLDER, "None");
        assert_eq!(SANDBOX_NOT_ENABLED_MESSAGE, "Sandbox is not enabled");
        assert_eq!(EXCLUDED_COMMANDS_TITLE, "Excluded Commands:");
        assert_eq!(FS_READ_TITLE, "Filesystem Read Restrictions:");
        assert_eq!(FS_WRITE_TITLE, "Filesystem Write Restrictions:");
        assert_eq!(NETWORK_TITLE_UNMANAGED, "Network Restrictions:");
        assert_eq!(NETWORK_TITLE_MANAGED, "Network Restrictions (Managed):");
        assert_eq!(UNIX_SOCKETS_TITLE, "Allowed Unix Sockets:");
    }
}
