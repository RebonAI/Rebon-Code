//! What an "allow always" answer means, with no dialog attached.
//!
//! Answering a permission prompt with "allow always" turns one tool call
//! into a rule: which rule, how it is scoped to this directory, when a
//! shell command may be generalized to a prefix, and where the rule is
//! written down. None of that needs a terminal — the background worker
//! answers the same prompts over IPC with no screen at all, and reaches
//! the same four entry points ([`add_allow_always_candidates`],
//! [`is_allow_always_option_id`], [`canonical_permission_option_id`],
//! [`persist_allow_always_rule`]).
//!
//! The modal that shows these options, and the keys that pick one, stay
//! in the binary's `tui::runner::permission_flow`; the worker never touched
//! them.

use std::path::Path;

use rebon_core::permission::{
    OutboundPermissionQuery, PermissionOptionKind, PermissionQueryOption,
};

pub const ULTRAPLAN_CEO_OPTION_ID: &str = "yes_continue_ceo";

pub const ULTRAPLAN_ULTRAWORK_OPTION_ID: &str = "yes_continue_ultrawork";

pub fn add_allow_always_candidates(outbound: &mut OutboundPermissionQuery, cwd: &str) {
    // Bash and MCP tools expand a single `allow_always` into multiple
    // scoped candidates (exact + generalized); WebFetch keeps a single
    // candidate but relabels it with the approved domain. Other tools
    // keep their single rule and don't need relabeling here.
    let expandable = outbound.tool_name == "Bash"
        || matches!(
            outbound.tool_name.as_str(),
            "Mcp" | "McpTool" | "MCPTool" | "Monitor" | "WebFetch"
        )
        || parse_mcp_proxy_name(&outbound.tool_name).is_some();
    if !expandable {
        return;
    }
    if !outbound.options.iter().any(|option| {
        option.option_id == "allow_always" && option.kind == PermissionOptionKind::AllowAlways
    }) {
        return;
    }

    let candidates =
        allow_always_rule_candidates(&outbound.tool_name, outbound.tool_input.as_ref(), cwd);
    if candidates.is_empty() {
        return;
    }

    if let Some(option) = outbound
        .options
        .iter_mut()
        .find(|option| option.option_id == "allow_always")
    {
        option.label = allow_always_candidate_label(&candidates[0]);
    }

    if candidates.len() <= 1 {
        return;
    }

    let insert_at = outbound
        .options
        .iter()
        .position(|option| option.option_id == "allow_always")
        .map(|idx| idx + 1)
        .unwrap_or(outbound.options.len());
    for candidate in candidates.into_iter().skip(1).rev() {
        // Queries can pass through here more than once (background
        // forwarding, detach/reattach) — never duplicate an option.
        if outbound
            .options
            .iter()
            .any(|option| option.option_id == candidate.option_id)
        {
            continue;
        }
        outbound.options.insert(
            insert_at,
            PermissionQueryOption {
                option_id: candidate.option_id.clone(),
                label: allow_always_candidate_label(&candidate),
                kind: PermissionOptionKind::AllowAlways,
            },
        );
    }
}

pub(crate) fn is_allow_always_option_id(option_id: &str) -> bool {
    option_id == "allow_always" || option_id == "allow_always_generalized"
}

pub(crate) fn canonical_permission_option_id(option_id: Option<String>) -> Option<String> {
    match option_id.as_deref() {
        Some("allow_always_generalized") => Some("allow_always".to_string()),
        Some(ULTRAPLAN_CEO_OPTION_ID) | Some(ULTRAPLAN_ULTRAWORK_OPTION_ID) => {
            Some("yes_default".to_string())
        }
        _ => option_id,
    }
}

/// Build a [`PermissionRuleValue`] from a pending permission query
/// and persist it both in the live [`PolicyStore`] (so subsequent
/// calls in this session are auto-allowed) and on disk in
/// `.rebon/settings.json` (so the rule survives restarts).
pub(crate) fn persist_allow_always_rule(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    policy_store: &rebon_core::policy::PolicyStore,
    cwd: &str,
    selected_option_id: Option<&str>,
) {
    use rebon_permissions::types::PermissionRuleSource;

    let rule_values = allow_always_rule_values(tool_name, tool_input, cwd, selected_option_id);
    if rule_values.is_empty() {
        tracing::warn!(
            tool = %tool_name,
            "skipping unsafe or unsupported allow_always rule"
        );
        return;
    }

    // 1. Update the live policy store (immediate in-session effect).
    for rule_value in &rule_values {
        policy_store.allow(
            &rebon_permissions::rule_value::permission_rule_value_to_string(rule_value),
            PermissionRuleSource::ProjectSettings,
        );
    }

    // 2. Persist to `.rebon/settings.json` on disk.
    let cwd_path = Path::new(cwd);
    for rule_value in &rule_values {
        if let Err(err) =
            crate::project_settings::add_project_permission_allow_rule(cwd_path, rule_value)
        {
            tracing::warn!(error = %err, "failed to persist allow_always rule to settings");
        }
    }
}

#[cfg(test)]
fn allow_always_rule_value(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: &str,
) -> Option<rebon_permissions::types::PermissionRuleValue> {
    allow_always_rule_values(tool_name, tool_input, cwd, Some("allow_always"))
        .into_iter()
        .next()
}

fn allow_always_rule_values(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: &str,
    selected_option_id: Option<&str>,
) -> Vec<rebon_permissions::types::PermissionRuleValue> {
    let candidates = allow_always_rule_candidates(tool_name, tool_input, cwd);
    let option_id = selected_option_id.unwrap_or("allow_always");
    candidates
        .into_iter()
        .find(|candidate| candidate.option_id == option_id)
        .map(|candidate| candidate.rules)
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AllowAlwaysRuleCandidate {
    option_id: String,
    rules: Vec<rebon_permissions::types::PermissionRuleValue>,
}

fn allow_always_rule_candidates(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: &str,
) -> Vec<AllowAlwaysRuleCandidate> {
    use rebon_permissions::types::PermissionRuleValue;

    if tool_name.is_empty() {
        return vec![];
    }

    // MCP tools carry their own scoping rules (server-qualified), so
    // handle them before the file/shell content match below.
    if let Some(mcp_candidates) = allow_always_mcp_candidates(tool_name, tool_input) {
        return mcp_candidates;
    }

    // The Agent prompt asks about directories the worker may reach, so
    // the stored grant is one path rule per requested root — never a
    // bare `Agent` rule, which would wave through every future spawn
    // regardless of where it points.
    if let Some(agent_candidates) = allow_always_agent_candidates(tool_name, tool_input, cwd) {
        return agent_candidates;
    }

    // File tools scope to the project directory; which tools those are is their own answer.
    let file_scoped = rebon_tool::tool_kind_for_name(tool_name)
        .touches_a_file()
        .then(|| format!("{}/**", cwd.replace('\\', "/")));
    let Some(rule_content) = (match tool_name {
        _ if file_scoped.is_some() => file_scoped,
        "Bash" | "PowerShell" => allow_always_shell_rule_content(tool_input, cwd),
        // Scope to the fetched host so approval never widens to the
        // whole web; no host (relative/garbled URL) → no rule.
        "WebFetch" => tool_input
            .and_then(|input| input.get("url"))
            .and_then(|url| url.as_str())
            .and_then(rebon_permissions::web_fetch_hostname)
            .map(|host| format!("domain:{host}")),
        "Monitor" => tool_input.and_then(|input| {
            if input.get("command").is_some() {
                allow_always_shell_rule_content(Some(input), cwd)
            } else {
                input
                    .get("ws")
                    .and_then(|url| url.as_str())
                    .and_then(rebon_permissions::web_fetch_hostname)
                    .map(|host| format!("domain:{host}"))
            }
        }),
        _ => None,
    }) else {
        return vec![];
    };

    let exact = AllowAlwaysRuleCandidate {
        option_id: "allow_always".to_string(),
        rules: vec![PermissionRuleValue::new(
            tool_name.to_string(),
            Some(rule_content),
        )],
    };

    if tool_name != "Bash" {
        return vec![exact];
    }

    let mut candidates = vec![exact];
    if let Some(generalized) = allow_always_shell_generalized_candidate(tool_name, tool_input, cwd)
    {
        candidates.push(generalized);
    }
    candidates
}

/// Build allow-always candidates for MCP tool calls. Returns `None`
/// for non-MCP tools so the caller falls through to file/shell handling.
///
/// Two shapes reach the permission layer:
/// - Proxy tools whose id is `mcp__<server>__<tool>` (one registered
///   tool per discovered MCP tool). The exact candidate is a rule on
///   the full tool name; the generalized candidate is the server scope
///   `mcp__<server>`, which covers every tool on that server.
/// - The generic `Mcp` facade whose input carries `{server, name}`.
///   The exact candidate is `Mcp(server:name)`; the generalized
///   candidate is `Mcp(server:*)`.
fn allow_always_mcp_candidates(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
) -> Option<Vec<AllowAlwaysRuleCandidate>> {
    use rebon_permissions::types::PermissionRuleValue;

    if let Some((server, tool)) = parse_mcp_proxy_name(tool_name) {
        let exact = AllowAlwaysRuleCandidate {
            option_id: "allow_always".to_string(),
            rules: vec![PermissionRuleValue::new(
                tool_name.to_string(),
                Option::<String>::None,
            )],
        };
        let mut candidates = vec![exact];
        // Only offer the server scope when we can name the tool — that
        // confirms `tool_name` is a real `mcp__server__tool`, not a bare
        // `mcp__server` already at server scope.
        if tool.is_some() {
            candidates.push(AllowAlwaysRuleCandidate {
                option_id: "allow_always_generalized".to_string(),
                rules: vec![PermissionRuleValue::new(
                    format!("mcp__{server}"),
                    Option::<String>::None,
                )],
            });
        }
        return Some(candidates);
    }

    if matches!(tool_name, "Mcp" | "McpTool" | "MCPTool") {
        let server = tool_input
            .and_then(|input| input.get("server"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|server| !server.is_empty())?;
        let name = tool_input
            .and_then(|input| input.get("name"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty());

        let mut candidates = Vec::new();
        if let Some(name) = name {
            candidates.push(AllowAlwaysRuleCandidate {
                option_id: "allow_always".to_string(),
                rules: vec![PermissionRuleValue::new(
                    "Mcp".to_string(),
                    Some(format!("{server}:{name}")),
                )],
            });
        }
        // The server-wide candidate keeps the generalized option id when
        // an exact one exists, otherwise it stands in as the primary
        // allow-always rule (e.g. when the call omitted a tool name).
        let generalized_option_id = if candidates.is_empty() {
            "allow_always"
        } else {
            "allow_always_generalized"
        };
        candidates.push(AllowAlwaysRuleCandidate {
            option_id: generalized_option_id.to_string(),
            rules: vec![PermissionRuleValue::new(
                "Mcp".to_string(),
                Some(format!("{server}:*")),
            )],
        });
        return Some(candidates);
    }

    None
}

/// Split a proxy tool name into `(server, Some(tool))`, or
/// `(server, None)` when `name` is already a bare `mcp__<server>` scope.
/// Returns `None` for any non-MCP-proxy name.
///
/// The tool is taken as the trailing `__`-delimited segment so server
/// names containing single underscores (`example_ai_Gmail`) round-trip
/// correctly.
fn parse_mcp_proxy_name(name: &str) -> Option<(String, Option<String>)> {
    let rest = name.strip_prefix("mcp__")?;
    if rest.is_empty() {
        return None;
    }
    match rest.rsplit_once("__") {
        Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
            Some((server.to_string(), Some(tool.to_string())))
        }
        // No inner `__`: a bare `mcp__<server>` server scope.
        None => Some((rest.to_string(), None)),
        // Degenerate forms like `mcp____tool` / `mcp__server__`.
        Some(_) => None,
    }
}

/// Build allow-always candidates for an Agent call. Returns `None` for
/// other tools so the caller falls through to the file/shell handling.
///
/// Only the roots the call actually names are granted, and each becomes
/// its own `Agent(<root>/**)` rule. The engine requires every path in a
/// later call to be covered before it honours the grant, so a directory
/// approved once here cannot smuggle a second, unapproved directory
/// through on the next spawn.
fn allow_always_agent_candidates(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: &str,
) -> Option<Vec<AllowAlwaysRuleCandidate>> {
    use rebon_permissions::types::PermissionRuleValue;

    if !matches!(tool_name, "Agent" | "AgentTool") {
        return None;
    }
    let input = tool_input?;
    let mut roots = Vec::new();
    let mut push = |value: Option<&str>| {
        let Some(root) = value.map(str::trim).filter(|root| !root.is_empty()) else {
            return;
        };
        let root = agent_rule_root(root, cwd);
        if !roots.contains(&root) {
            roots.push(root);
        }
    };
    push(input.get("cwd").and_then(|value| value.as_str()));
    if let Some(allowed) = input
        .get("allowed_roots")
        .and_then(|value| value.as_array())
    {
        for root in allowed {
            push(root.as_str());
        }
    }
    if roots.is_empty() {
        return Some(Vec::new());
    }
    Some(vec![AllowAlwaysRuleCandidate {
        option_id: "allow_always".to_string(),
        rules: roots
            .into_iter()
            .map(|root| PermissionRuleValue::new(tool_name.to_string(), Some(root)))
            .collect(),
    }])
}

/// Normalize one requested agent root into rule content: absolute,
/// forward-slashed, and suffixed with `/**` so it covers the subtree.
fn agent_rule_root(root: &str, cwd: &str) -> String {
    let path = std::path::Path::new(root);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::Path::new(cwd).join(path)
    };
    let normalized = absolute
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    format!("{normalized}/**")
}

fn allow_always_shell_generalized_candidate(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: &str,
) -> Option<AllowAlwaysRuleCandidate> {
    use rebon_permissions::shell_runtime::BashShape;
    use rebon_permissions::types::PermissionRuleValue;

    let command = normalized_shell_command(tool_input, cwd)?;
    let (segments, allow_first_word_prefix) =
        match rebon_permissions::shell_runtime::parse_bash_shape(command) {
            BashShape::Simple(argv) => (vec![argv], true),
            BashShape::SafeAndChain(segments) => (segments, false),
            BashShape::UnsafeComplex => return None,
        };

    let empty_envs = std::collections::BTreeSet::new();
    let mut rules = Vec::new();
    for segment in segments {
        let segment_command = segment.join(" ");
        let prefix = rebon_permissions::shell_runtime::get_simple_command_prefix(
            &segment_command,
            &empty_envs,
            &empty_envs,
            false,
        )
        .or_else(|| {
            allow_first_word_prefix.then(|| {
                rebon_permissions::shell_runtime::get_first_word_prefix(
                    &segment_command,
                    &empty_envs,
                    &empty_envs,
                    false,
                )
            })?
        })?;
        if shell_prefix_starts_with_bare_shell(&prefix) {
            return None;
        }
        let rule_content = format!("{prefix}:*");
        if !rules.iter().any(|rule: &PermissionRuleValue| {
            rule.rule_content.as_deref() == Some(rule_content.as_str())
        }) {
            rules.push(PermissionRuleValue::new(
                tool_name.to_string(),
                Some(rule_content),
            ));
        }
    }

    if rules.is_empty() {
        return None;
    }

    Some(AllowAlwaysRuleCandidate {
        option_id: "allow_always_generalized".to_string(),
        rules,
    })
}

fn allow_always_candidate_label(candidate: &AllowAlwaysRuleCandidate) -> String {
    if let Some(label) = allow_always_mcp_candidate_label(candidate) {
        return label;
    }
    if let Some(host) = candidate
        .rules
        .first()
        .filter(|rule| matches!(rule.tool_name.as_str(), "Monitor" | "WebFetch"))
        .and_then(|rule| rule.rule_content.as_deref())
        .and_then(|content| content.strip_prefix("domain:"))
    {
        return format!("Allow always for {host}");
    }
    let rule_names = candidate
        .rules
        .iter()
        .filter_map(|rule| rule.rule_content.as_deref())
        .map(generalized_shell_rule_label)
        .collect::<Vec<_>>();
    if candidate.option_id == "allow_always_generalized" && !rule_names.is_empty() {
        format!("Allow always {} commands", rule_names.join(" + "))
    } else {
        "Allow always exact command".to_string()
    }
}

/// Human-readable label for an MCP allow-always candidate, or `None`
/// when the candidate isn't MCP-shaped (caller falls back to shell/file
/// labels).
fn allow_always_mcp_candidate_label(candidate: &AllowAlwaysRuleCandidate) -> Option<String> {
    let first = candidate.rules.first()?;

    // Generic `Mcp` facade: rule content is `server:tool` / `server:*`.
    if first.tool_name == "Mcp" {
        let content = first.rule_content.as_deref()?;
        let (server, tool) = content.split_once(':')?;
        return Some(if tool == "*" {
            format!("Allow always all {server} tools")
        } else {
            format!("Allow always {server}: {tool}")
        });
    }

    // Proxy tool: name is `mcp__server__tool` (exact) or `mcp__server`
    // (server scope); rule content is empty.
    let (server, tool) = parse_mcp_proxy_name(&first.tool_name)?;
    Some(match tool {
        Some(tool) => format!("Allow always {server}: {tool}"),
        None => format!("Allow always all {server} tools"),
    })
}

fn generalized_shell_rule_label(rule_content: &str) -> String {
    rule_content
        .strip_suffix(":*")
        .unwrap_or(rule_content)
        .to_string()
}

fn allow_always_shell_rule_content(
    tool_input: Option<&serde_json::Value>,
    cwd: &str,
) -> Option<String> {
    let command = normalized_shell_command(tool_input, cwd)?;
    if command.is_empty() {
        return None;
    }
    if shell_command_is_bare_shell(command) {
        None
    } else {
        Some(command.to_string())
    }
}

fn normalized_shell_command<'a>(
    tool_input: Option<&'a serde_json::Value>,
    cwd: &str,
) -> Option<&'a str> {
    let command = tool_input?.get("command").and_then(|v| v.as_str())?;
    Some(
        rebon_core::policy::strip_cwd_prefix(command, cwd)
            .unwrap_or(command)
            .trim(),
    )
}

fn shell_prefix_starts_with_bare_shell(prefix: &str) -> bool {
    rebon_permissions::shell_runtime::first_shell_word(prefix)
        .is_some_and(rebon_permissions::shell_runtime::is_bare_shell_prefix)
}

fn shell_command_is_bare_shell(command: &str) -> bool {
    let first = rebon_permissions::shell_runtime::first_shell_word(command).unwrap_or("");
    !first.is_empty()
        && command[first.len()..].trim().is_empty()
        && rebon_permissions::shell_runtime::is_bare_shell_prefix(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    use rebon_core::permission::{
        OutboundPermissionQuery, PermissionOptionKind, PermissionQueryOption,
    };
    use serde_json::json;
    use tokio::sync::oneshot;

    #[test]
    fn allow_always_web_fetch_rule_scopes_to_the_fetched_host() {
        let rule = allow_always_rule_value(
            "WebFetch",
            Some(&json!({"url": "https://Docs.RS/tokio/latest"})),
            "F:/dev/project",
        )
        .expect("a URL with a host should produce a domain rule");
        assert_eq!(rule.tool_name, "WebFetch");
        assert_eq!(rule.rule_content.as_deref(), Some("domain:docs.rs"));

        let candidates = allow_always_rule_candidates(
            "WebFetch",
            Some(&json!({"url": "https://docs.rs/tokio"})),
            "F:/dev/project",
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            allow_always_candidate_label(&candidates[0]),
            "Allow always for docs.rs"
        );

        // No host → no rule, so "allow always" degrades to a one-shot
        // allow instead of persisting something over-broad.
        assert!(
            allow_always_rule_value("WebFetch", Some(&json!({"url": "not a url"})), "F:/dev")
                .is_none()
        );
    }

    #[test]
    fn allow_always_monitor_rules_are_source_scoped() {
        let command_rule = allow_always_rule_value(
            "Monitor",
            Some(&json!({
                "command": "cargo test -p rebon-core",
                "description": "engine tests"
            })),
            "F:/dev/project",
        )
        .expect("a command monitor should produce an exact command rule");
        assert_eq!(command_rule.tool_name, "Monitor");
        assert_eq!(
            command_rule.rule_content.as_deref(),
            Some("cargo test -p rebon-core")
        );

        let websocket_candidates = allow_always_rule_candidates(
            "Monitor",
            Some(&json!({
                "ws": "wss://Events.Example.com/private?token=secret",
                "description": "deployment events"
            })),
            "F:/dev/project",
        );
        assert_eq!(websocket_candidates.len(), 1);
        assert_eq!(websocket_candidates[0].rules[0].tool_name, "Monitor");
        assert_eq!(
            websocket_candidates[0].rules[0].rule_content.as_deref(),
            Some("domain:events.example.com")
        );
        assert_eq!(
            allow_always_candidate_label(&websocket_candidates[0]),
            "Allow always for events.example.com"
        );
    }

    #[test]
    fn allow_always_shell_rule_defaults_to_exact_command() {
        let rule = allow_always_rule_value(
            "Bash",
            Some(&json!({"command": "cargo test -p rebon-core"})),
            "F:/dev/project",
        )
        .expect("cargo test should produce an allow rule");

        assert_eq!(rule.tool_name, "Bash");
        assert_eq!(
            rule.rule_content.as_deref(),
            Some("cargo test -p rebon-core")
        );
    }

    #[test]
    fn allow_always_shell_rule_strips_cwd_for_exact_command() {
        let rule = allow_always_rule_value(
            "Bash",
            Some(&json!({"command": r#"cd "F:\dev\project" && cargo test --workspace"#})),
            "F:/dev/project",
        )
        .expect("cd wrapper should be normalized");

        assert_eq!(rule.rule_content.as_deref(), Some("cargo test --workspace"));
    }

    #[test]
    fn allow_always_shell_rule_preserves_compound_commands_exactly() {
        let rule = allow_always_rule_value(
            "Bash",
            Some(&json!({"command": "cargo test && rm -rf target"})),
            "F:/dev/project",
        )
        .expect("compound commands should only allow the exact command");

        assert_eq!(
            rule.rule_content.as_deref(),
            Some("cargo test && rm -rf target")
        );
    }

    #[test]
    fn allow_always_shell_rule_rejects_bare_shell_roots() {
        assert!(allow_always_rule_value(
            "Bash",
            Some(&json!({"command": "bash"})),
            "F:/dev/project",
        )
        .is_none());
        assert!(allow_always_rule_value(
            "PowerShell",
            Some(&json!({"command": "powershell"})),
            "F:/dev/project",
        )
        .is_none());
    }

    #[test]
    fn allow_always_shell_rule_preserves_unprefixable_commands_exactly() {
        let rule = allow_always_rule_value(
            "Bash",
            Some(&json!({"command": "bash -lc 'cargo test'"})),
            "F:/dev/project",
        )
        .expect("non-root shell invocation should only allow the exact command");

        assert_eq!(rule.rule_content.as_deref(), Some("bash -lc 'cargo test'"));
    }

    #[test]
    fn allow_always_candidates_include_exact_and_generalized_same_safe_chain() {
        let candidates = allow_always_rule_candidates(
            "Bash",
            Some(&json!({"command": "cargo test -p a && cargo test -p b"})),
            "F:/dev/project",
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].option_id, "allow_always");
        assert_eq!(
            candidates[0].rules[0].rule_content.as_deref(),
            Some("cargo test -p a && cargo test -p b")
        );
        assert_eq!(candidates[1].option_id, "allow_always_generalized");
        assert_eq!(candidates[1].rules.len(), 1);
        assert_eq!(
            candidates[1].rules[0].rule_content.as_deref(),
            Some("cargo test:*")
        );
    }

    /// "Always allow" on an agent directory prompt must grant those
    /// directories, not the Agent tool. A bare `Agent` rule would wave
    /// through every future spawn no matter where it pointed.
    #[test]
    fn allow_always_candidates_scope_agent_grants_to_the_requested_roots() {
        // Absolute on the platform under test: a Windows drive path is
        // relative on unix and gets joined onto the cwd, which is not the
        // scoping behavior this test is about.
        #[cfg(windows)]
        let (project, tasks) = ("F:/dev/project", "C:/Users/alice/.rebon/tasks");
        #[cfg(not(windows))]
        let (project, tasks) = ("/dev/project", "/home/alice/.rebon/tasks");
        let candidates = allow_always_rule_candidates(
            "Agent",
            Some(&json!({
                "cwd": project,
                "allowed_roots": [project, tasks],
            })),
            project,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].option_id, "allow_always");
        let contents = candidates[0]
            .rules
            .iter()
            .map(|rule| rule.rule_content.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            vec![format!("{project}/**"), format!("{tasks}/**")]
        );
        assert!(candidates[0]
            .rules
            .iter()
            .all(|rule| rule.tool_name == "Agent"));
    }

    #[test]
    fn allow_always_candidates_resolve_relative_agent_roots_against_cwd() {
        let candidates = allow_always_rule_candidates(
            "Agent",
            Some(&json!({ "allowed_roots": ["sub/dir"] })),
            "F:/dev/project",
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].rules[0].rule_content.as_deref(),
            Some("F:/dev/project/sub/dir/**")
        );
    }

    /// No directory named → nothing to scope a grant to. Offering a
    /// rule here would have to be an unscoped `Agent` rule.
    #[test]
    fn allow_always_candidates_are_empty_for_a_pathless_agent_call() {
        let candidates = allow_always_rule_candidates(
            "Agent",
            Some(&json!({ "description": "go", "prompt": "do it" })),
            "F:/dev/project",
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn allow_always_candidates_include_mixed_safe_chain_rules() {
        let candidates = allow_always_rule_candidates(
            "Bash",
            Some(&json!({"command": "cargo fmt --all && cargo test -p a"})),
            "F:/dev/project",
        );

        assert_eq!(candidates.len(), 2);
        let generalized = &candidates[1];
        assert_eq!(generalized.option_id, "allow_always_generalized");
        assert_eq!(generalized.rules.len(), 2);
        assert_eq!(
            generalized.rules[0].rule_content.as_deref(),
            Some("cargo fmt:*")
        );
        assert_eq!(
            generalized.rules[1].rule_content.as_deref(),
            Some("cargo test:*")
        );
    }

    #[test]
    fn allow_always_candidates_do_not_generalize_unsafe_chain() {
        let candidates = allow_always_rule_candidates(
            "Bash",
            Some(&json!({"command": "cargo test -p a && rm -rf target"})),
            "F:/dev/project",
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].option_id, "allow_always");
        assert_eq!(
            candidates[0].rules[0].rule_content.as_deref(),
            Some("cargo test -p a && rm -rf target")
        );
    }

    #[test]
    fn allow_always_candidates_generalize_quoted_and_argument() {
        let candidates = allow_always_rule_candidates(
            "Bash",
            Some(&json!({"command": "cargo test 'foo && bar'"})),
            "F:/dev/project",
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[1].rules[0].rule_content.as_deref(),
            Some("cargo test:*")
        );
    }

    #[test]
    fn add_allow_always_candidates_relabels_exact_and_adds_generalized_option() {
        let (response_tx, _response_rx) = oneshot::channel();
        let mut outbound = OutboundPermissionQuery {
            id: 1,
            tool_name: "Bash".into(),
            tool_call_id: "tool-bash-1".into(),
            session_id: "sess-1".into(),
            title: "Run command".into(),
            message: "Bash(command=\"cargo test -p a && cargo test -p b\")".into(),
            tool_input: Some(json!({"command": "cargo test -p a && cargo test -p b"})),
            metadata: None,
            options: vec![
                PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionQueryOption {
                    option_id: "allow_always".into(),
                    label: "allow always".into(),
                    kind: PermissionOptionKind::AllowAlways,
                },
                PermissionQueryOption {
                    option_id: "reject_once".into(),
                    label: "Reject once".into(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
            response_tx,
        };

        add_allow_always_candidates(&mut outbound, "F:/dev/project");

        assert_eq!(outbound.options[1].option_id, "allow_always");
        assert_eq!(outbound.options[1].label, "Allow always exact command");
        assert_eq!(outbound.options[2].option_id, "allow_always_generalized");
        assert_eq!(
            outbound.options[2].label,
            "Allow always cargo test commands"
        );
    }

    #[test]
    fn add_allow_always_candidates_is_idempotent() {
        // Queries pass through expansion more than once on the
        // background-forward and detach/reattach paths; a second call
        // must not duplicate the generalized option.
        let (response_tx, _response_rx) = oneshot::channel();
        let mut outbound = OutboundPermissionQuery {
            id: 1,
            tool_name: "Bash".into(),
            tool_call_id: "tool-bash-2".into(),
            session_id: "sess-1".into(),
            title: "Run command".into(),
            message: "Bash(command=\"cargo test -p a && cargo test -p b\")".into(),
            tool_input: Some(json!({"command": "cargo test -p a && cargo test -p b"})),
            metadata: None,
            options: vec![
                PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionQueryOption {
                    option_id: "allow_always".into(),
                    label: "allow always".into(),
                    kind: PermissionOptionKind::AllowAlways,
                },
            ],
            response_tx,
        };

        add_allow_always_candidates(&mut outbound, "F:/dev/project");
        let expanded_once = outbound
            .options
            .iter()
            .map(|option| (option.option_id.clone(), option.label.clone()))
            .collect::<Vec<_>>();
        add_allow_always_candidates(&mut outbound, "F:/dev/project");
        let expanded_twice = outbound
            .options
            .iter()
            .map(|option| (option.option_id.clone(), option.label.clone()))
            .collect::<Vec<_>>();

        assert_eq!(expanded_once.len(), 3);
        assert_eq!(expanded_once, expanded_twice);
    }

    #[test]
    fn canonicalizes_generalized_and_ultraplan_options_for_engine() {
        assert_eq!(
            canonical_permission_option_id(Some("allow_always_generalized".to_string())),
            Some("allow_always".to_string())
        );
        assert_eq!(
            canonical_permission_option_id(Some(ULTRAPLAN_CEO_OPTION_ID.to_string())),
            Some("yes_default".to_string())
        );
        assert_eq!(
            canonical_permission_option_id(Some(ULTRAPLAN_ULTRAWORK_OPTION_ID.to_string())),
            Some("yes_default".to_string())
        );
    }

    #[test]
    fn parse_mcp_proxy_name_splits_server_and_tool() {
        assert_eq!(
            parse_mcp_proxy_name("mcp__context-engine__search_context"),
            Some((
                "context-engine".to_string(),
                Some("search_context".to_string())
            ))
        );
        // Server with single underscores keeps them; tool is the trailing
        // segment.
        assert_eq!(
            parse_mcp_proxy_name("mcp__example_ai_Gmail__authenticate"),
            Some((
                "example_ai_Gmail".to_string(),
                Some("authenticate".to_string())
            ))
        );
        // Bare server scope.
        assert_eq!(
            parse_mcp_proxy_name("mcp__context-engine"),
            Some(("context-engine".to_string(), None))
        );
        // Non-MCP / degenerate names.
        assert_eq!(parse_mcp_proxy_name("Bash"), None);
        assert_eq!(parse_mcp_proxy_name("mcp__"), None);
        assert_eq!(parse_mcp_proxy_name("mcp____tool"), None);
        assert_eq!(parse_mcp_proxy_name("mcp__server__"), None);
    }

    #[test]
    fn allow_always_candidates_for_mcp_proxy_tool() {
        let candidates = allow_always_rule_candidates(
            "mcp__context-engine__search_context",
            Some(&json!({"query": "hello"})),
            "F:/dev/project",
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].option_id, "allow_always");
        assert_eq!(candidates[0].rules.len(), 1);
        assert_eq!(
            candidates[0].rules[0].tool_name,
            "mcp__context-engine__search_context"
        );
        assert_eq!(candidates[0].rules[0].rule_content, None);
        assert_eq!(
            allow_always_candidate_label(&candidates[0]),
            "Allow always context-engine: search_context"
        );

        assert_eq!(candidates[1].option_id, "allow_always_generalized");
        assert_eq!(candidates[1].rules[0].tool_name, "mcp__context-engine");
        assert_eq!(candidates[1].rules[0].rule_content, None);
        assert_eq!(
            allow_always_candidate_label(&candidates[1]),
            "Allow always all context-engine tools"
        );
    }

    #[test]
    fn allow_always_candidates_for_generic_mcp_facade() {
        let candidates = allow_always_rule_candidates(
            "Mcp",
            Some(&json!({"server": "notes", "name": "fetch", "arguments": {}})),
            "F:/dev/project",
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].option_id, "allow_always");
        assert_eq!(candidates[0].rules[0].tool_name, "Mcp");
        assert_eq!(
            candidates[0].rules[0].rule_content.as_deref(),
            Some("notes:fetch")
        );
        assert_eq!(
            allow_always_candidate_label(&candidates[0]),
            "Allow always notes: fetch"
        );

        assert_eq!(candidates[1].option_id, "allow_always_generalized");
        assert_eq!(
            candidates[1].rules[0].rule_content.as_deref(),
            Some("notes:*")
        );
        assert_eq!(
            allow_always_candidate_label(&candidates[1]),
            "Allow always all notes tools"
        );
    }

    #[test]
    fn allow_always_value_for_mcp_proxy_selects_chosen_candidate() {
        let exact = allow_always_rule_values(
            "mcp__context-engine__search_context",
            Some(&json!({"query": "x"})),
            "F:/dev/project",
            Some("allow_always"),
        );
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].tool_name, "mcp__context-engine__search_context");

        let server_scope = allow_always_rule_values(
            "mcp__context-engine__search_context",
            Some(&json!({"query": "x"})),
            "F:/dev/project",
            Some("allow_always_generalized"),
        );
        assert_eq!(server_scope.len(), 1);
        assert_eq!(server_scope[0].tool_name, "mcp__context-engine");
    }

    #[test]
    fn add_allow_always_candidates_expands_mcp_proxy_options() {
        let (response_tx, _response_rx) = oneshot::channel();
        let mut outbound = OutboundPermissionQuery {
            id: 1,
            tool_name: "mcp__context-engine__search_context".into(),
            tool_call_id: "tool-mcp-1".into(),
            session_id: "sess-1".into(),
            title: "Call MCP tool".into(),
            message: "Call search_context on MCP server context-engine".into(),
            tool_input: Some(json!({"query": "hello"})),
            metadata: None,
            options: vec![
                PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionQueryOption {
                    option_id: "allow_always".into(),
                    label: "allow always".into(),
                    kind: PermissionOptionKind::AllowAlways,
                },
                PermissionQueryOption {
                    option_id: "reject_once".into(),
                    label: "Reject once".into(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
            response_tx,
        };

        add_allow_always_candidates(&mut outbound, "F:/dev/project");

        assert_eq!(outbound.options[1].option_id, "allow_always");
        assert_eq!(
            outbound.options[1].label,
            "Allow always context-engine: search_context"
        );
        assert_eq!(outbound.options[2].option_id, "allow_always_generalized");
        assert_eq!(
            outbound.options[2].label,
            "Allow always all context-engine tools"
        );
        // The reject option stays last.
        assert_eq!(outbound.options[3].option_id, "reject_once");
    }
}
