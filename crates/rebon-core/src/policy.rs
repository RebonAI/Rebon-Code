//! Rules-based permission policy — bridges `rebon-permissions`
//! into the runtime tool-dispatch path.
//!
//! Before this module, `rebon-core` only used the runtime
//! [`PermissionBroker`] chain (default → ACP reverse-RPC). Stored
//! `.claude/settings.json`-style rules (`Bash(ls:*)`,
//! `Edit(src/**)`, …) were never consulted, so every tool call
//! with `needs_permission() == true` went straight to the Ask path
//! no matter how many allow-rules the user had already saved.
//!
//! This module plugs that gap with three pieces:
//!
//! 1. [`PolicyStore`] — owns a set of [`PermissionRule`]s
//!    reconstructed from whatever storage the caller uses
//!    (settings file, CLI flag, session one-shots). The store is
//!    ordered: deny rules take precedence over allow rules inside
//!    the same tool.
//! 2. [`PolicyEvaluator`] — given a tool name + input, matches
//!    against the store's rules and returns a
//!    [`PolicyOutcome::AutoAllow`], [`PolicyOutcome::AutoDeny`], or
//!    [`PolicyOutcome::PassThrough`]. Matching is tool-specific:
//!    Bash rules match command prefixes, Read/Write/Edit rules
//!    match file paths, MCP rules match `server:tool` pairs, and
//!    everything else falls back to name-only matching.
//! 3. [`RulesBasedPermissionBroker`] — implements
//!    [`PermissionBroker`]. First consults the policy:
//!    - `AutoAllow`: rewrites the incoming decision into
//!      `PermissionDecision::allow(input)` and delegates to the
//!      inner broker.
//!    - `AutoDeny`: returns [`ToolError::PermissionDenied`]
//!      without touching the delegate.
//!    - `PassThrough`: forwards the original decision to the
//!      delegate (typically [`crate::AcpPermissionBroker`] or
//!      [`rebon_tool::DenyAskPermissionBroker`]).
//!
//! The policy layer is **additive**: no existing behaviour changes
//! unless the caller explicitly wraps their broker in a
//! [`RulesBasedPermissionBroker`]. The CLI path builds one from
//! the loaded settings.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use rebon_permissions::{
    powershell_shape::{cmdlet_names_match, parse_powershell_shape},
    rule_value::permission_rule_value_from_string,
    shell_runtime::{first_shell_word, is_bare_shell_prefix, parse_bash_shape, BashShape},
    types::{PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue},
    web_fetch_hostname,
};
use rebon_tool::{PermissionBroker, Tool, ToolContext};
use rebon_tools_core::{PermissionDecision, ToolError, ToolKind, ToolResult};
use serde_json::Value;

/// Outcome of evaluating a tool call against a [`PolicyStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// At least one matching allow rule, no overriding deny — the
    /// tool may run without prompting.
    AutoAllow {
        /// The rule that matched (for audit logging).
        matched: PermissionRuleValue,
    },
    /// A deny rule matched — the tool must not run.
    AutoDeny {
        /// The rule that matched (for audit logging).
        matched: PermissionRuleValue,
    },
    /// No rule matched; fall back to the normal prompt path.
    PassThrough,
}

/// Host-owned, fallible lookup of a session's live store using the request's
/// immutable cwd. The engine never loads settings or substitutes a fallback
/// store when this resolver refuses the scope.
pub type PolicyStoreResolver = Arc<dyn Fn(&str, &str) -> Result<PolicyStore, String> + Send + Sync>;

/// In-memory store of stored permission rules.
///
/// Thread-safe and cloneable: every clone points at the same
/// underlying store, so callers can hand the same store to
/// multiple brokers or inject updates from a settings watcher
/// without rebuilding anything.
#[derive(Debug, Clone, Default)]
pub struct PolicyStore {
    inner: Arc<RwLock<PolicyInner>>,
}

#[derive(Debug, Default)]
struct PolicyInner {
    rules: Vec<PermissionRule>,
}

impl PolicyStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a store from raw rule strings (`Bash(ls:*)`,
    /// `Edit(src/**)`, …). Each string is parsed via
    /// [`permission_rule_value_from_string`]; the caller supplies
    /// the behaviour + source tag for every entry.
    pub fn from_strings<I, S>(
        strings: I,
        behavior: PermissionBehavior,
        source: PermissionRuleSource,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let store = Self::new();
        for raw in strings {
            let rule_value = permission_rule_value_from_string(raw.as_ref());
            store.push_rule(PermissionRule {
                source,
                rule_behavior: behavior,
                rule_value,
            });
        }
        store
    }

    /// Append a pre-built rule.
    pub fn push_rule(&self, rule: PermissionRule) {
        self.try_push_rule(rule).expect("policy store poisoned");
    }

    /// Fallible live update for hosts that must reject a permission response
    /// instead of acknowledging a grant after the store has failed.
    pub fn try_push_rule(&self, rule: PermissionRule) -> Result<(), String> {
        if rule.rule_behavior == PermissionBehavior::Allow
            && is_unsafe_shell_allow_rule(&rule.rule_value)
        {
            tracing::warn!(
                tool = %rule.rule_value.tool_name,
                content = ?rule.rule_value.rule_content,
                "ignoring unsafe broad shell allow rule"
            );
            return Ok(());
        }
        let mut guard = self.inner.write().map_err(|_| "policy store poisoned")?;
        guard.rules.push(rule);
        Ok(())
    }

    /// Append an allow rule from a raw string.
    pub fn allow(&self, raw: impl AsRef<str>, source: PermissionRuleSource) -> PermissionRuleValue {
        let rule_value = permission_rule_value_from_string(raw.as_ref());
        self.push_rule(PermissionRule {
            source,
            rule_behavior: PermissionBehavior::Allow,
            rule_value: rule_value.clone(),
        });
        rule_value
    }

    /// Append a deny rule from a raw string.
    pub fn deny(&self, raw: impl AsRef<str>, source: PermissionRuleSource) -> PermissionRuleValue {
        let rule_value = permission_rule_value_from_string(raw.as_ref());
        self.push_rule(PermissionRule {
            source,
            rule_behavior: PermissionBehavior::Deny,
            rule_value: rule_value.clone(),
        });
        rule_value
    }

    /// Remove every rule. Useful when the caller re-syncs from an
    /// authoritative store (e.g. after a settings file reload).
    pub fn clear(&self) {
        let mut guard = self.inner.write().expect("policy store poisoned");
        guard.rules.clear();
    }

    /// Number of registered rules.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("policy store poisoned")
            .rules
            .len()
    }

    /// Whether the store has no rules.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot every rule in insertion order. Primarily for tests
    /// and diagnostics.
    pub fn snapshot(&self) -> Vec<PermissionRule> {
        self.inner
            .read()
            .expect("policy store poisoned")
            .rules
            .clone()
    }
}

/// Stateless evaluator that runs a tool call against a
/// [`PolicyStore`].
pub struct PolicyEvaluator {
    store: PolicyStore,
}

impl PolicyEvaluator {
    /// Construct an evaluator over `store`.
    pub fn new(store: PolicyStore) -> Self {
        Self { store }
    }

    /// The live policy store this evaluator reads rules from.
    pub fn store(&self) -> &PolicyStore {
        &self.store
    }

    /// Evaluate a tool call and return the outcome.
    ///
    /// `cwd` is the session working directory — when supplied, Bash
    /// commands that start with `cd <cwd> && <actual>` and PowerShell
    /// commands that start with `Set-Location <cwd>; <actual>` are
    /// normalized to just `<actual>` before matching, so a redundant
    /// `cd` into the working directory does not hide the real command.
    ///
    /// Matching precedence:
    /// 1. If any deny rule matches the call → [`PolicyOutcome::AutoDeny`].
    /// 2. Else if any allow rule matches → [`PolicyOutcome::AutoAllow`].
    /// 3. Else → [`PolicyOutcome::PassThrough`].
    ///
    /// Within each behaviour class, the first matching rule wins —
    /// later rules of the same behaviour are ignored (they would
    /// produce the same outcome anyway).
    pub fn evaluate(&self, tool_name: &str, input: &Value, cwd: Option<&str>) -> PolicyOutcome {
        // Ignore a leading working-directory wrapper when it points
        // at the session cwd, so rules match the command that follows.
        let input = normalize_shell_input(tool_name, input, cwd);

        let guard = self.store.inner.read().expect("policy store poisoned");
        let mut matching_allow: Option<PermissionRuleValue> = None;
        for rule in &guard.rules {
            if !rule_applies_to_tool(&rule.rule_value.tool_name, tool_name) {
                continue;
            }
            if !matches_input(
                &rule.rule_value,
                rule.rule_behavior,
                tool_name,
                &input,
                &guard.rules,
            ) {
                continue;
            }
            match rule.rule_behavior {
                PermissionBehavior::Deny => {
                    return PolicyOutcome::AutoDeny {
                        matched: rule.rule_value.clone(),
                    };
                }
                PermissionBehavior::Allow => {
                    if matching_allow.is_none() {
                        matching_allow = Some(rule.rule_value.clone());
                    }
                }
                PermissionBehavior::Ask => {
                    // Ask rules are treated as pass-through here —
                    // they won't auto-allow but they also don't
                    // auto-deny. A follow-up module may promote them
                    // to a "force prompt" outcome.
                }
            }
        }
        if let Some(matched) = matching_allow {
            PolicyOutcome::AutoAllow { matched }
        } else {
            PolicyOutcome::PassThrough
        }
    }
}

fn rule_applies_to_tool(rule_tool: &str, call_tool: &str) -> bool {
    if rule_tool.is_empty() {
        return false;
    }
    if rule_tool.eq_ignore_ascii_case(call_tool) {
        return true;
    }
    // MCP server-scope rule: `mcp__<server>` auto-allows every tool
    // that server exposes, i.e. any `mcp__<server>__<tool>` proxy call.
    // Exact `mcp__<server>__<tool>` rules already matched by name above.
    if mcp_server_scope_matches(rule_tool, call_tool) {
        return true;
    }
    // Accept the aliases the runtime registers on each tool — the tools
    // declare those themselves, so this reads them rather than repeating
    // them.
    if let Some(facts) = rebon_tools_core::builtin_tool_facts_for_name(call_tool) {
        if facts.matches(rule_tool) {
            return true;
        }
        // A rule named for the class covers every member of it. `Edit` is the
        // spelling of the file-edit umbrella, so `Edit(src/**)` auto-allows
        // Write, MultiEdit and NotebookEdit under `src/` too. A deny rule
        // folds the same way, which is the half that matters: `deny:
        // Edit(secrets/**)` would be worth nothing if MultiEdit slipped out
        // from under it — and it is the kind, not a list, that decides who is
        // under it.
        let umbrella = match facts.kind {
            ToolKind::FileEdit => ["Edit", "FileEdit", "FileEditTool"].as_slice(),
            ToolKind::FileRead => ["FileRead", "FileReadTool"].as_slice(),
            _ => [].as_slice(),
        };
        if umbrella.contains(&rule_tool) {
            return true;
        }
    }
    // Synonyms no tool declares as an alias: the `FileWrite` spelling of the
    // write umbrella, and `Mcp`, whose kind is `Other` so the table above
    // does not carry it. `Skill` is `Other` too but has a row of its own,
    // so `SkillTool` already matched above.
    match (call_tool, rule_tool) {
        ("Write", "FileWrite") => true,
        ("Mcp", "McpTool" | "MCPTool") => true,
        _ => false,
    }
}

/// Does an MCP server-scope rule (`mcp__<server>`) cover a proxy tool
/// call (`mcp__<server>__<tool>`)?
///
/// A rule whose remainder after `mcp__` still contains `__` is itself a
/// specific tool name, not a server scope — those are handled by the
/// exact-name match in [`rule_applies_to_tool`], so this returns false
/// for them. The trailing `__` boundary check stops a server prefix
/// from matching a longer server name (`mcp__ctx` must not cover
/// `mcp__ctx-engine__tool`).
fn mcp_server_scope_matches(rule_tool: &str, call_tool: &str) -> bool {
    let Some(server) = rule_tool.strip_prefix("mcp__") else {
        return false;
    };
    if server.is_empty() || server.contains("__") {
        return false;
    }
    call_tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(server))
        .is_some_and(|tail| tail.starts_with("__") && tail.len() > 2)
}

fn is_unsafe_shell_allow_rule(rule_value: &PermissionRuleValue) -> bool {
    if rule_value.rule_content.is_none() && is_bare_shell_wildcard_tool_name(&rule_value.tool_name)
    {
        return true;
    }
    if !is_shell_tool_name(&rule_value.tool_name) {
        return false;
    }

    let Some(rule_content) = rule_value.rule_content.as_deref() else {
        return true;
    };
    let rule_content = rule_content.trim();
    if rule_content.is_empty() || rule_content == "*" {
        return true;
    }

    is_unsafe_shell_command_rule(rule_content)
}

fn is_unsafe_shell_command_rule(rule_content: &str) -> bool {
    if let Some(head) = rule_content
        .strip_suffix(":*")
        .or_else(|| rule_content.strip_suffix(':'))
    {
        return first_shell_word(head).is_some_and(is_bare_shell_prefix);
    }

    let first = first_shell_word(rule_content).unwrap_or("");
    rule_content[first.len()..].trim().is_empty() && is_bare_shell_prefix(first)
}

fn is_bare_shell_wildcard_tool_name(tool_name: &str) -> bool {
    let mut words = tool_name.split_ascii_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let Some(second) = words.next() else {
        return false;
    };
    words.next().is_none() && second == "*" && is_bare_shell_prefix(first)
}

fn is_shell_tool_name(tool_name: &str) -> bool {
    [
        "Bash",
        "BashTool",
        "PowerShell",
        "PowerShellTool",
        "Monitor",
        "MonitorTool",
    ]
    .iter()
    .any(|name| tool_name.eq_ignore_ascii_case(name))
}

fn matches_input(
    rule_value: &PermissionRuleValue,
    behavior: PermissionBehavior,
    tool_name: &str,
    input: &Value,
    rules: &[PermissionRule],
) -> bool {
    let Some(rule_content) = rule_value.rule_content.as_deref() else {
        // Rule without content matches any call to the tool.
        return true;
    };
    if rule_content.is_empty() {
        return true;
    }
    // A file tool's rule is a path rule, read out of whichever input field
    // that tool names its target with — `file_path` for the editors,
    // `notebook_path` for NotebookEdit. Reading the wrong field would leave an
    // `Edit(...)` rule evaluating against an empty path, so the field comes
    // from the tool rather than from a list here.
    if let Some(field) = rebon_tools_core::file_target_field_for_name(tool_name) {
        let path = input.get(field).and_then(|v| v.as_str()).unwrap_or("");
        return matches_path_rule(rule_content, path);
    }
    match tool_name {
        // Two shells, two grammars. Sharing one arm meant PowerShell
        // commands were tokenised by POSIX rules, so `\` in every Windows
        // path was read as an escape and the argv a rule was compared
        // against did not exist — see
        // [`rebon_permissions::powershell_shape`].
        "Bash" | "BashTool" => {
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            matches_bash_rule(rule_content, behavior, command, rules, tool_name)
        }
        "PowerShell" | "PowerShellTool" => {
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            matches_powershell_rule(rule_content, behavior, command, rules, tool_name)
        }
        "Mcp" | "McpTool" | "MCPTool" => {
            let server = input.get("server").and_then(|v| v.as_str()).unwrap_or("");
            let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
            matches_mcp_rule(rule_content, server, name)
        }
        // The Agent prompt is about directories the worker may touch, so
        // its rules are path rules. EVERY requested path must fall under
        // the rule: a rule authorizing one directory must not silently
        // clear a call that also reaches somewhere else. Without this arm
        // an `Agent(...)` rule would fall through to the generic branch
        // and be compared against `name` — the teammate name — so a
        // stored grant could never match the call that created it.
        "Agent" | "AgentTool" => {
            let mut targets = Vec::new();
            if let Some(cwd) = input.get("cwd").and_then(Value::as_str) {
                targets.push(cwd);
            }
            if let Some(roots) = input.get("allowed_roots").and_then(Value::as_array) {
                targets.extend(roots.iter().filter_map(Value::as_str));
            }
            let targets = targets
                .into_iter()
                .map(str::trim)
                .filter(|target| !target.is_empty())
                .collect::<Vec<_>>();
            if targets.is_empty() {
                // Nothing to compare against: keep deny/ask matching
                // (fail closed) but never widen an allow rule to every
                // Agent call.
                behavior != PermissionBehavior::Allow
            } else {
                targets
                    .iter()
                    .all(|target| matches_path_rule(rule_content, target))
            }
        }
        "Glob" | "GlobTool" | "Grep" | "GrepTool" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            matches_literal_or_star(rule_content, pattern)
        }
        "WebFetch" | "WebFetchTool" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            matches_web_fetch_rule(rule_content, behavior, url)
        }
        "Monitor" | "MonitorTool" => {
            if let Some(command) = input.get("command").and_then(Value::as_str) {
                matches_bash_rule(rule_content, behavior, command, rules, tool_name)
            } else {
                let url = input.get("ws").and_then(Value::as_str).unwrap_or("");
                matches_web_fetch_rule(rule_content, behavior, url)
            }
        }
        _ => {
            // Unknown tool — treat the rule content as an exact
            // literal match against a best-guess `target` field.
            // Without a comparable field the rule content cannot be
            // evaluated: deny/ask rules keep matching (fail closed),
            // but an allow rule must never widen to every call of
            // the tool just because the input shape is unknown.
            if let Some(target) = input
                .get("target")
                .and_then(|v| v.as_str())
                .or_else(|| input.get("name").and_then(|v| v.as_str()))
            {
                matches_literal_or_star(rule_content, target)
            } else {
                behavior != PermissionBehavior::Allow
            }
        }
    }
}

/// Match a Bash rule like `ls:*`, `git:log`, `cargo test:*`, or plain `ls`.
///
/// Semantics:
/// - `prefix:*` matches any command whose leading words equal `prefix`.
/// - `prefix:suffix` requires the remaining words after `prefix` to equal `suffix`.
/// - A rule without a `:` matches only when the entire command
///   (trimmed) equals the rule content.
fn matches_bash_rule(
    rule: &str,
    behavior: PermissionBehavior,
    command: &str,
    rules: &[PermissionRule],
    tool_name: &str,
) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }

    if matches_exact_bash_rule(rule, command) {
        return true;
    }

    let Some((head, tail)) = rule.split_once(':') else {
        return false;
    };
    let shape = parse_bash_shape(command);
    match behavior {
        PermissionBehavior::Deny => match shape {
            BashShape::Simple(argv) => matches_bash_prefix_rule(head, tail, &argv),
            BashShape::SafeAndChain(segments) => segments
                .iter()
                .any(|argv| matches_bash_prefix_rule(head, tail, argv)),
            BashShape::UnsafeComplex => false,
        },
        PermissionBehavior::Allow => match shape {
            BashShape::Simple(argv) => matches_bash_prefix_rule(head, tail, &argv),
            BashShape::SafeAndChain(segments) => {
                bash_chain_segments_allowed(&segments, rules, tool_name)
            }
            BashShape::UnsafeComplex => false,
        },
        PermissionBehavior::Ask => match shape {
            BashShape::Simple(argv) => matches_bash_prefix_rule(head, tail, &argv),
            BashShape::SafeAndChain(segments) => segments
                .iter()
                .any(|argv| matches_bash_prefix_rule(head, tail, argv)),
            BashShape::UnsafeComplex => false,
        },
    }
}

fn matches_exact_bash_rule(rule: &str, command: &str) -> bool {
    !rule.contains(':') && command == rule
}

/// The PowerShell twin of [`matches_bash_rule`].
///
/// Same decision table — the difference is entirely in how the command
/// becomes an argv, and in the fact that `rm` and `Remove-Item` are the same
/// cmdlet. Both differences matter in opposite directions: the tokeniser
/// makes allow rules able to match at all, and the alias table stops a deny
/// rule from being walked past by typing the short name.
fn matches_powershell_rule(
    rule: &str,
    behavior: PermissionBehavior,
    command: &str,
    rules: &[PermissionRule],
    tool_name: &str,
) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }

    if matches_exact_bash_rule(rule, command) {
        return true;
    }

    let Some((head, tail)) = rule.split_once(':') else {
        return false;
    };
    let shape = parse_powershell_shape(command);
    match behavior {
        // Deny and Ask hit on any segment: one segment being forbidden is
        // enough to stop the whole line.
        PermissionBehavior::Deny | PermissionBehavior::Ask => match shape {
            BashShape::Simple(argv) => matches_powershell_prefix_rule(head, tail, &argv),
            BashShape::SafeAndChain(segments) => segments
                .iter()
                .any(|argv| matches_powershell_prefix_rule(head, tail, argv)),
            BashShape::UnsafeComplex => false,
        },
        // Allow needs *every* segment covered: a chain is only permitted
        // when nothing in it is unaccounted for.
        PermissionBehavior::Allow => match shape {
            BashShape::Simple(argv) => matches_powershell_prefix_rule(head, tail, &argv),
            BashShape::SafeAndChain(segments) => {
                powershell_chain_segments_allowed(&segments, rules, tool_name)
            }
            BashShape::UnsafeComplex => false,
        },
    }
}

fn powershell_chain_segments_allowed(
    segments: &[Vec<String>],
    rules: &[PermissionRule],
    tool_name: &str,
) -> bool {
    segments.iter().all(|segment| {
        rules.iter().any(|rule| {
            rule.rule_behavior == PermissionBehavior::Allow
                && rule_applies_to_tool(&rule.rule_value.tool_name, tool_name)
                && rule
                    .rule_value
                    .rule_content
                    .as_deref()
                    .is_some_and(|rule_content| match rule_content.split_once(':') {
                        Some((head, tail)) => matches_powershell_prefix_rule(head, tail, segment),
                        None => matches_powershell_exact_segment_rule(rule_content, segment),
                    })
        })
    })
}

fn matches_powershell_exact_segment_rule(rule: &str, argv: &[String]) -> bool {
    let rule_tokens = rule.split_ascii_whitespace().collect::<Vec<_>>();
    rule_tokens.len() == argv.len()
        && rule_tokens
            .iter()
            .zip(argv.iter())
            .enumerate()
            .all(|(index, (rule, command))| powershell_token_matches(index, rule, command))
}

fn matches_powershell_prefix_rule(head: &str, tail: &str, argv: &[String]) -> bool {
    let head_tokens = head.split_ascii_whitespace().collect::<Vec<_>>();
    if head_tokens.is_empty() || argv.len() < head_tokens.len() {
        return false;
    }
    if !head_tokens
        .iter()
        .zip(argv.iter())
        .enumerate()
        .all(|(index, (head, command))| powershell_token_matches(index, head, command))
    {
        return false;
    }
    if tail == "*" || tail.is_empty() {
        return true;
    }
    let rest = argv
        .iter()
        .skip(head_tokens.len())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    rest == tail
}

/// Compare one token of a rule against one token of a command.
///
/// Alias resolution applies to the **command name only**. `rm` in first
/// position is `Remove-Item`; `rm` as an argument is the literal string `rm`,
/// and rewriting it would make a rule match a path it does not name.
fn powershell_token_matches(index: usize, rule_token: &str, command_token: &str) -> bool {
    if index == 0 {
        cmdlet_names_match(rule_token, command_token)
    } else {
        rule_token.eq_ignore_ascii_case(command_token)
    }
}

fn bash_chain_segments_allowed(
    segments: &[Vec<String>],
    rules: &[PermissionRule],
    tool_name: &str,
) -> bool {
    segments.iter().all(|segment| {
        rules.iter().any(|rule| {
            rule.rule_behavior == PermissionBehavior::Allow
                && rule_applies_to_tool(&rule.rule_value.tool_name, tool_name)
                && rule
                    .rule_value
                    .rule_content
                    .as_deref()
                    .is_some_and(|rule_content| matches_bash_segment_rule(rule_content, segment))
        })
    })
}

fn matches_bash_segment_rule(rule: &str, argv: &[String]) -> bool {
    if let Some((head, tail)) = rule.split_once(':') {
        matches_bash_prefix_rule(head, tail, argv)
    } else {
        matches_bash_exact_segment_rule(rule, argv)
    }
}

fn matches_bash_exact_segment_rule(rule: &str, argv: &[String]) -> bool {
    let rule_tokens = rule.split_ascii_whitespace().collect::<Vec<_>>();
    rule_tokens.len() == argv.len()
        && rule_tokens
            .iter()
            .zip(argv.iter())
            .all(|(rule, command)| rule.eq_ignore_ascii_case(command))
}

fn matches_bash_prefix_rule(head: &str, tail: &str, argv: &[String]) -> bool {
    let head_tokens = head.split_ascii_whitespace().collect::<Vec<_>>();
    if head_tokens.is_empty() || argv.len() < head_tokens.len() {
        return false;
    }
    if !head_tokens
        .iter()
        .zip(argv.iter())
        .all(|(head, command)| head.eq_ignore_ascii_case(command))
    {
        return false;
    }
    if tail == "*" || tail.is_empty() {
        return true;
    }
    let rest = argv
        .iter()
        .skip(head_tokens.len())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    rest == tail
}

/// Match a file-path rule against a tool `file_path` argument.
///
/// Supported forms:
/// - Exact string match (`/tmp/foo.rs`).
/// - Trailing `/**` → any path under the prefix.
/// - Trailing `/*` → any immediate child of the prefix.
///
/// Both the rule and path are normalized to forward slashes so
/// rules written on Unix still match Windows-style `\` paths and
/// vice-versa.
fn matches_path_rule(rule: &str, path: &str) -> bool {
    let rule = rule.replace('\\', "/");
    let path = path.replace('\\', "/");
    if rule == "*" || rule == "**" {
        return true;
    }
    if let Some(prefix) = rule.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = rule.strip_suffix("/*") {
        if let Some(rest) = path.strip_prefix(&format!("{prefix}/")) {
            return !rest.contains('/');
        }
        return false;
    }
    rule == path
}

fn matches_mcp_rule(rule: &str, server: &str, name: &str) -> bool {
    if let Some((r_server, r_name)) = rule.split_once(':') {
        if !r_server.is_empty() && !r_server.eq_ignore_ascii_case(server) {
            return false;
        }
        if r_name == "*" || r_name.is_empty() {
            return true;
        }
        r_name.eq_ignore_ascii_case(name)
    } else {
        rule.eq_ignore_ascii_case(server)
    }
}

fn matches_literal_or_star(rule: &str, target: &str) -> bool {
    if rule == "*" {
        return true;
    }
    rule == target
}

/// Match a WebFetch rule like `domain:docs.rs` against the call's URL.
///
/// Only the `domain:<host>` form names a host to compare — exact host,
/// case-insensitive (the host the user approved, not its subdomains).
/// Any other content, including a bare `*` or a `WebFetch(evil.com)`
/// written without the `domain:` prefix, cannot be evaluated against
/// the URL, so it resolves like the unknown-tool branch above: an allow
/// rule must never widen to the whole web, while deny/ask keep matching
/// and fail closed. Bare `WebFetch` rules (no content) are handled by
/// the caller's empty-content fast path like every other tool.
fn matches_web_fetch_rule(rule: &str, behavior: PermissionBehavior, url: &str) -> bool {
    let rule_host = rule
        .strip_prefix("domain:")
        .map(|host| host.trim().trim_end_matches('.'))
        .filter(|host| !host.is_empty() && !host.contains('*'));
    let Some(rule_host) = rule_host else {
        return behavior != PermissionBehavior::Allow;
    };
    let Some(host) = web_fetch_hostname(url) else {
        return false;
    };
    host.trim_end_matches('.').eq_ignore_ascii_case(rule_host)
}

// Working-directory prefix stripping lives in `rebon-permissions` so the
// surfaces that *author* allow-always rules (TUI, ACP) strip exactly what
// this evaluator strips. A rule recorded from the raw `cd <cwd> && cargo
// test` text can never match the stripped command otherwise.
pub use rebon_permissions::shell_runtime::strip_cwd_prefix;
pub(crate) use rebon_permissions::shell_runtime::strip_powershell_cwd_prefix;

/// If `input` is a Bash/PowerShell command with a working-directory
/// wrapper matching `cwd`, return a new input `Value` with the prefix
/// stripped. Otherwise return the original value unchanged.
fn normalize_shell_input(tool_name: &str, input: &Value, cwd: Option<&str>) -> Value {
    let cwd = match cwd {
        Some(c) if !c.is_empty() => c,
        _ => return input.clone(),
    };
    match tool_name {
        "Bash" | "PowerShell" | "BashTool" | "PowerShellTool" => {}
        _ => return input.clone(),
    }
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return input.clone(),
    };
    let tail = match tool_name {
        "Bash" | "BashTool" => strip_cwd_prefix(command, cwd),
        "PowerShell" | "PowerShellTool" => {
            strip_powershell_cwd_prefix(command, cwd).or_else(|| strip_cwd_prefix(command, cwd))
        }
        _ => None,
    };
    if let Some(tail) = tail {
        let mut out = input.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("command".into(), serde_json::Value::String(tail.to_owned()));
        }
        out
    } else {
        input.clone()
    }
}

/// [`PermissionBroker`] that evaluates a [`PolicyStore`] before
/// delegating to an inner broker.
pub struct RulesBasedPermissionBroker {
    evaluator: PolicyEvaluator,
    delegate: Arc<dyn PermissionBroker>,
}

impl std::fmt::Debug for RulesBasedPermissionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RulesBasedPermissionBroker")
            .field("rule_count", &self.evaluator.store.len())
            .finish()
    }
}

impl RulesBasedPermissionBroker {
    /// Construct a broker from a policy store + delegate.
    pub fn new(store: PolicyStore, delegate: Arc<dyn PermissionBroker>) -> Self {
        Self {
            evaluator: PolicyEvaluator::new(store),
            delegate,
        }
    }

    /// Borrow the inner delegate — useful for diagnostics and when
    /// callers want to add another layer of middleware (e.g. an
    /// audit log) on top of the delegate chain.
    pub fn delegate(&self) -> &Arc<dyn PermissionBroker> {
        &self.delegate
    }

    /// The live policy store backing this broker's rule evaluation.
    /// Clones share the same underlying rules, so a sub-agent broker
    /// built from this store sees interactive `allow_always` grants
    /// the moment they are written.
    pub fn store(&self) -> &PolicyStore {
        self.evaluator.store()
    }
}

#[async_trait]
impl PermissionBroker for RulesBasedPermissionBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        match self
            .evaluator
            .evaluate(tool.id().as_str(), &input, context.cwd())
        {
            PolicyOutcome::AutoAllow { matched } => {
                tracing::debug!(
                    tool = tool.id().as_str(),
                    rule = ?matched,
                    "rules-based broker: auto-allow from stored rule"
                );
                let allow = PermissionDecision::allow(input.clone());
                self.delegate.resolve(tool, input, context, allow).await
            }
            PolicyOutcome::AutoDeny { matched } => Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: format!(
                    "denied by stored rule: {}{}",
                    matched.tool_name,
                    matched
                        .rule_content
                        .as_ref()
                        .map(|c| format!("({c})"))
                        .unwrap_or_default()
                ),
            }),
            PolicyOutcome::PassThrough => {
                self.delegate.resolve(tool, input, context, decision).await
            }
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rebon_tool::{DenyAskPermissionBroker, Tool};
    use rebon_tools_core::{PermissionRequest, ToolId, ToolInputSchema};
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn poisoned_live_store_refuses_grant_without_panicking() {
        let store = PolicyStore::new();
        let worker = store.clone();
        assert!(std::thread::spawn(move || {
            let _guard = worker.inner.write().unwrap();
            panic!("poison live store");
        })
        .join()
        .is_err());
        assert!(store
            .try_push_rule(PermissionRule {
                source: PermissionRuleSource::ProjectSettings,
                rule_behavior: PermissionBehavior::Allow,
                rule_value: PermissionRuleValue::new("Read", None::<String>),
            })
            .is_err());
    }

    #[test]
    fn bash_rule_star_matches_any_command_with_prefix() {
        assert!(matches_bash_rule(
            "ls:*",
            PermissionBehavior::Allow,
            "ls",
            &[],
            "Bash"
        ));
        assert!(matches_bash_rule(
            "ls:*",
            PermissionBehavior::Allow,
            "ls -la",
            &[],
            "Bash"
        ));
        assert!(matches_bash_rule(
            "ls:*",
            PermissionBehavior::Allow,
            "ls /tmp",
            &[],
            "Bash"
        ));
        assert!(!matches_bash_rule(
            "ls:*",
            PermissionBehavior::Allow,
            "cat foo",
            &[],
            "Bash"
        ));
    }

    fn powershell_allows(rule: &str, command: &str) -> bool {
        matches_powershell_rule(rule, PermissionBehavior::Allow, command, &[], "PowerShell")
    }

    fn powershell_denies(rule: &str, command: &str) -> bool {
        matches_powershell_rule(rule, PermissionBehavior::Deny, command, &[], "PowerShell")
    }

    #[test]
    fn a_powershell_rule_matches_a_windows_path() {
        // The regression this whole split exists for. Under the POSIX
        // tokeniser `C:\temp\notes.txt` became `C:tempnotes.txt`, so this
        // rule could never match and the user was prompted every time for a
        // command they had explicitly allowed.
        assert!(powershell_allows(
            r"Get-Content:C:\temp\notes.txt",
            r"Get-Content C:\temp\notes.txt"
        ));
        assert!(powershell_allows(
            "Get-Content:*",
            r"Get-Content C:\temp\notes.txt"
        ));
    }

    #[test]
    fn the_bash_matcher_still_gets_posix_escaping() {
        // The other half of the split: Bash must keep reading `\` as an
        // escape, so this is *not* a regression that the two now disagree.
        assert!(matches_bash_rule(
            r"echo:ab",
            PermissionBehavior::Allow,
            r"echo a\b",
            &[],
            "Bash"
        ));
    }

    #[test]
    fn a_powershell_alias_cannot_walk_past_a_deny_rule() {
        // Three characters would otherwise bypass a rule written for the
        // cmdlet.
        assert!(powershell_denies("Remove-Item:*", "rm C:\\work\\x"));
        assert!(powershell_denies("Remove-Item:*", "del C:\\work\\x"));
        // And a rule written with the alias catches the cmdlet.
        assert!(powershell_denies("rm:*", r"Remove-Item C:\work\x"));
    }

    #[test]
    fn an_alias_in_argument_position_stays_literal() {
        // Only the command name resolves. Rewriting an argument would make
        // a rule match a path it does not name.
        assert!(powershell_allows("Get-Content:rm", "Get-Content rm"));
        assert!(!powershell_allows(
            "Get-Content:Remove-Item",
            "Get-Content rm"
        ));
    }

    #[test]
    fn powershell_matching_is_case_insensitive() {
        assert!(powershell_allows("get-process:*", "Get-Process -Name pwsh"));
        assert!(powershell_allows("Get-Process:*", "GET-PROCESS"));
    }

    #[test]
    fn a_quoted_powershell_argument_matches_its_unquoted_rule() {
        assert!(powershell_allows(
            r"Get-Content:C:\my files\a.txt",
            "Get-Content 'C:\\my files\\a.txt'"
        ));
    }

    #[test]
    fn a_complex_powershell_command_matches_nothing() {
        // The safety property, in the matcher rather than the tokeniser: a
        // pipeline or a sub-expression could hide a second command behind a
        // rule written for the first, so nothing matches and the user is
        // asked.
        for command in [
            "Get-Process | Remove-Item",
            "Get-Process; Remove-Item x",
            "Remove-Item $(Get-Content list.txt)",
            "Get-ChildItem | ForEach-Object { Remove-Item $_ }",
        ] {
            assert!(
                !powershell_allows("Get-Process:*", command),
                "{command:?} matched an allow rule"
            );
            assert!(
                !powershell_allows("*", command),
                "{command:?} matched a wildcard allow rule"
            );
        }
    }

    #[test]
    fn a_powershell_rule_without_a_colon_is_an_exact_match() {
        assert!(powershell_allows("Get-Process", "Get-Process"));
        assert!(!powershell_allows("Get-Process", "Get-Process -Name x"));
    }

    #[test]
    fn a_powershell_suffix_rule_requires_the_whole_tail() {
        assert!(powershell_allows("git:log", "git log"));
        assert!(!powershell_allows("git:log", "git status"));
        assert!(!powershell_allows("git:log", "git log --oneline"));
    }

    #[test]
    fn an_empty_powershell_command_matches_nothing() {
        assert!(!powershell_allows("Get-Process:*", ""));
        assert!(!powershell_allows("Get-Process:*", "   "));
    }

    #[test]
    fn a_powershell_allow_chain_needs_every_segment_covered() {
        let rules = vec![
            PermissionRule {
                rule_value: PermissionRuleValue::new("PowerShell", Some("Get-Process:*")),
                rule_behavior: PermissionBehavior::Allow,
                source: PermissionRuleSource::CliArg,
            },
            PermissionRule {
                rule_value: PermissionRuleValue::new("PowerShell", Some("Get-Service:*")),
                rule_behavior: PermissionBehavior::Allow,
                source: PermissionRuleSource::CliArg,
            },
        ];

        assert!(matches_powershell_rule(
            "Get-Process:*",
            PermissionBehavior::Allow,
            "Get-Process && Get-Service",
            &rules,
            "PowerShell"
        ));
        // The second half is not covered by any allow rule, so the chain is
        // not allowed on the strength of the first half matching.
        assert!(!matches_powershell_rule(
            "Get-Process:*",
            PermissionBehavior::Allow,
            "Get-Process && Remove-Item x",
            &rules,
            "PowerShell"
        ));
    }

    #[test]
    fn a_powershell_deny_hits_on_any_segment_of_a_chain() {
        assert!(powershell_denies(
            "Remove-Item:*",
            "Get-Process && Remove-Item x"
        ));
    }

    #[test]
    fn bash_rule_exact_suffix_requires_match() {
        assert!(matches_bash_rule(
            "git:log",
            PermissionBehavior::Allow,
            "git log",
            &[],
            "Bash"
        ));
        assert!(!matches_bash_rule(
            "git:log",
            PermissionBehavior::Allow,
            "git status",
            &[],
            "Bash"
        ));
        assert!(!matches_bash_rule(
            "git:log",
            PermissionBehavior::Allow,
            "git log --oneline",
            &[],
            "Bash"
        ));
    }

    #[test]
    fn bash_rule_without_colon_is_exact_match() {
        assert!(matches_bash_rule(
            "ls",
            PermissionBehavior::Allow,
            "ls",
            &[],
            "Bash"
        ));
        assert!(!matches_bash_rule(
            "ls",
            PermissionBehavior::Allow,
            "ls -la",
            &[],
            "Bash"
        ));
    }

    /// An `Agent(<root>/**)` grant authorizes directories, so it must
    /// cover EVERY directory the next call names. Covering one of two
    /// would let an approved root drag an unapproved one in with it.
    #[test]
    fn agent_rule_requires_every_requested_root_to_be_covered() {
        let rule =
            PermissionRuleValue::new("Agent".to_string(), Some("C:/home/.rebon/**".to_string()));
        let covered = json!({
            "cwd": "C:/home/.rebon/tasks",
            "allowed_roots": ["C:/home/.rebon/tasks", "C:/home/.rebon/scratch"],
        });
        assert!(matches_input(
            &rule,
            PermissionBehavior::Allow,
            "Agent",
            &covered,
            &[]
        ));

        let partly_covered = json!({
            "allowed_roots": ["C:/home/.rebon/tasks", "F:/work/project"],
        });
        assert!(!matches_input(
            &rule,
            PermissionBehavior::Allow,
            "Agent",
            &partly_covered,
            &[]
        ));
    }

    /// The Agent input's `name` is a teammate name. Before Agent had its
    /// own arm the generic branch compared path rules against it, so a
    /// stored grant could never match the call that created it.
    #[test]
    fn agent_rule_is_not_matched_against_the_teammate_name() {
        let rule =
            PermissionRuleValue::new("Agent".to_string(), Some("C:/home/.rebon/**".to_string()));
        let named = json!({
            "name": "C:/home/.rebon/**",
            "allowed_roots": ["F:/work/project"],
        });
        assert!(!matches_input(
            &rule,
            PermissionBehavior::Allow,
            "Agent",
            &named,
            &[]
        ));
    }

    /// A call that names no directory at all has nothing to compare
    /// against: allow rules must not widen to it, deny/ask still bite.
    #[test]
    fn agent_rule_without_paths_never_widens_an_allow() {
        let rule =
            PermissionRuleValue::new("Agent".to_string(), Some("C:/home/.rebon/**".to_string()));
        let pathless = json!({ "description": "go" });
        assert!(!matches_input(
            &rule,
            PermissionBehavior::Allow,
            "Agent",
            &pathless,
            &[]
        ));
        assert!(matches_input(
            &rule,
            PermissionBehavior::Deny,
            "Agent",
            &pathless,
            &[]
        ));
    }

    #[test]
    fn path_rule_double_star_matches_any_descendant() {
        assert!(matches_path_rule("src/**", "src/foo.rs"));
        assert!(matches_path_rule("src/**", "src/nested/bar.rs"));
        assert!(matches_path_rule("src/**", "src"));
        assert!(!matches_path_rule("src/**", "other.rs"));
    }

    #[test]
    fn path_rule_normalizes_backslashes_for_windows_paths() {
        // Rule with forward slashes, path with backslashes (Windows).
        assert!(matches_path_rule(
            "F:/dev/project/**",
            "F:\\dev\\project\\src\\file.rs"
        ));
        assert!(matches_path_rule(
            "F:/dev/project/**",
            "F:/dev/project/src/file.rs"
        ));
        // Rule with backslashes, path with forward slashes.
        assert!(matches_path_rule(
            "F:\\dev\\project/**",
            "F:/dev/project/src/file.rs"
        ));
        // Mismatch still works.
        assert!(!matches_path_rule(
            "F:/dev/other/**",
            "F:\\dev\\project\\file.rs"
        ));
    }

    #[test]
    fn path_rule_single_star_matches_immediate_children_only() {
        assert!(matches_path_rule("src/*", "src/foo.rs"));
        assert!(!matches_path_rule("src/*", "src/nested/bar.rs"));
    }

    #[test]
    fn mcp_rule_matches_server_and_tool() {
        assert!(matches_mcp_rule("notes:*", "notes", "fetch"));
        assert!(matches_mcp_rule("notes:fetch", "notes", "fetch"));
        assert!(!matches_mcp_rule("notes:fetch", "notes", "create"));
        assert!(matches_mcp_rule("notes", "notes", "anything"));
    }

    #[test]
    fn mcp_server_scope_rule_covers_proxy_tool_calls() {
        assert!(mcp_server_scope_matches(
            "mcp__context-engine",
            "mcp__context-engine__search_context"
        ));
        // Server with single underscores is still a scope.
        assert!(mcp_server_scope_matches(
            "mcp__claude_ai_Gmail",
            "mcp__claude_ai_Gmail__authenticate"
        ));
        // A full tool rule is not a server scope (handled by exact match).
        assert!(!mcp_server_scope_matches(
            "mcp__context-engine__search_context",
            "mcp__context-engine__search_context"
        ));
        // Prefix of a longer server name must not match.
        assert!(!mcp_server_scope_matches(
            "mcp__ctx",
            "mcp__ctx-engine__search_context"
        ));
        // Non-MCP rule names never match as a scope.
        assert!(!mcp_server_scope_matches("Bash", "mcp__x__y"));
        assert!(!mcp_server_scope_matches("mcp__x", "Bash"));
    }

    #[test]
    fn evaluator_scopes_web_fetch_rules_to_the_exact_domain() {
        let store = PolicyStore::new();
        store.allow(
            "WebFetch(domain:docs.rs)",
            PermissionRuleSource::ProjectSettings,
        );
        let evaluator = PolicyEvaluator::new(store);

        assert!(matches!(
            evaluator.evaluate("WebFetch", &json!({"url": "https://docs.rs/tokio"}), None),
            PolicyOutcome::AutoAllow { .. }
        ));
        assert!(matches!(
            evaluator.evaluate("WebFetch", &json!({"url": "https://DOCS.RS/serde"}), None),
            PolicyOutcome::AutoAllow { .. }
        ));
        // Other hosts — including subdomains and suffix look-alikes —
        // are not covered by the exact-domain rule.
        for url in [
            "https://example.com/",
            "https://api.docs.rs/x",
            "https://evil-docs.rs.attacker.com/",
        ] {
            assert_eq!(
                evaluator.evaluate("WebFetch", &json!({"url": url}), None),
                PolicyOutcome::PassThrough,
                "{url}"
            );
        }
    }

    #[test]
    fn evaluator_scopes_monitor_rules_by_source_without_cross_tool_grants() {
        let store = PolicyStore::new();
        store.allow(
            "Monitor(domain:events.example.com)",
            PermissionRuleSource::ProjectSettings,
        );
        store.allow(
            "Monitor(cargo test -p rebon-core)",
            PermissionRuleSource::ProjectSettings,
        );
        let evaluator = PolicyEvaluator::new(store);

        assert!(matches!(
            evaluator.evaluate(
                "Monitor",
                &json!({"ws": "wss://events.example.com/stream"}),
                None
            ),
            PolicyOutcome::AutoAllow { .. }
        ));
        assert!(matches!(
            evaluator.evaluate(
                "Monitor",
                &json!({"command": "cargo test -p rebon-core"}),
                None
            ),
            PolicyOutcome::AutoAllow { .. }
        ));
        assert_eq!(
            evaluator.evaluate(
                "Monitor",
                &json!({"ws": "wss://other.example.com/stream"}),
                None
            ),
            PolicyOutcome::PassThrough
        );
        assert_eq!(
            evaluator.evaluate(
                "Monitor",
                &json!({"command": "cargo test -p rebon-cli"}),
                None
            ),
            PolicyOutcome::PassThrough
        );
        assert_eq!(
            evaluator.evaluate(
                "WebFetch",
                &json!({"url": "https://events.example.com/stream"}),
                None
            ),
            PolicyOutcome::PassThrough
        );
        assert_eq!(
            evaluator.evaluate(
                "Bash",
                &json!({"command": "cargo test -p rebon-core"}),
                None
            ),
            PolicyOutcome::PassThrough
        );
    }

    #[test]
    fn web_fetch_rules_without_domain_form_never_widen_to_the_whole_web() {
        // The generic unknown-tool fallback used to auto-match any
        // input without a `target`/`name` field; WebFetch must not
        // inherit that behavior for contentful rules. (`WebFetch(*)`
        // is out of scope here: the rule parser folds `Tool(*)` into
        // a bare whole-tool allow for every tool by design.)
        for rule in ["WebFetch(docs.rs)", "WebFetch(domain:*)"] {
            let store = PolicyStore::new();
            store.allow(rule, PermissionRuleSource::ProjectSettings);
            let evaluator = PolicyEvaluator::new(store);
            assert_eq!(
                evaluator.evaluate("WebFetch", &json!({"url": "https://example.com/"}), None),
                PolicyOutcome::PassThrough,
                "{rule}"
            );
        }
    }

    #[test]
    fn web_fetch_deny_rule_without_domain_form_still_fails_closed() {
        // `WebFetch(evil.com)` is what a user writes when they forget
        // the `domain:` prefix. It names no comparable host, so it must
        // fail closed like every other unevaluable deny rule rather
        // than silently permitting the fetch it was written to stop.
        for rule in ["WebFetch(evil.com)", "WebFetch(domain:*)"] {
            let store = PolicyStore::new();
            store.deny(rule, PermissionRuleSource::ProjectSettings);
            let evaluator = PolicyEvaluator::new(store);
            assert!(
                matches!(
                    evaluator.evaluate("WebFetch", &json!({"url": "https://evil.com/"}), None),
                    PolicyOutcome::AutoDeny { .. }
                ),
                "{rule}"
            );
        }
        // A well-formed deny rule still only covers its own host.
        let store = PolicyStore::new();
        store.deny(
            "WebFetch(domain:evil.com)",
            PermissionRuleSource::ProjectSettings,
        );
        let evaluator = PolicyEvaluator::new(store);
        assert!(matches!(
            evaluator.evaluate("WebFetch", &json!({"url": "https://evil.com/x"}), None),
            PolicyOutcome::AutoDeny { .. }
        ));
        assert_eq!(
            evaluator.evaluate("WebFetch", &json!({"url": "https://docs.rs/x"}), None),
            PolicyOutcome::PassThrough
        );
    }

    #[test]
    fn web_fetch_domain_rule_follows_the_host_the_request_will_contact() {
        // `\` terminates the authority for special schemes, so this URL
        // reaches evil.com. An allow rule for docs.rs must not cover it,
        // and a deny rule for evil.com must still catch it.
        let url = "https://evil.com\\@docs.rs/";

        let store = PolicyStore::new();
        store.allow(
            "WebFetch(domain:docs.rs)",
            PermissionRuleSource::ProjectSettings,
        );
        let evaluator = PolicyEvaluator::new(store);
        assert_eq!(
            evaluator.evaluate("WebFetch", &json!({"url": url}), None),
            PolicyOutcome::PassThrough
        );

        let store = PolicyStore::new();
        store.deny(
            "WebFetch(domain:evil.com)",
            PermissionRuleSource::ProjectSettings,
        );
        let evaluator = PolicyEvaluator::new(store);
        assert!(matches!(
            evaluator.evaluate("WebFetch", &json!({"url": url}), None),
            PolicyOutcome::AutoDeny { .. }
        ));
    }

    #[test]
    fn unknown_tool_allow_rule_with_content_never_widens_without_a_target() {
        let store = PolicyStore::new();
        store.allow("WebSearch(rust)", PermissionRuleSource::ProjectSettings);
        let evaluator = PolicyEvaluator::new(store);
        // No `target`/`name` on the input — a contentful allow rule
        // cannot be evaluated, so it must not auto-allow the call.
        assert_eq!(
            evaluator.evaluate("WebSearch", &json!({"query": "rust"}), None),
            PolicyOutcome::PassThrough
        );
        // With a comparable field the literal match still applies.
        assert!(matches!(
            evaluator.evaluate("WebSearch", &json!({"target": "rust"}), None),
            PolicyOutcome::AutoAllow { .. }
        ));
        // A bare whole-tool allow keeps matching any input.
        let store = PolicyStore::new();
        store.allow("WebSearch", PermissionRuleSource::ProjectSettings);
        let evaluator = PolicyEvaluator::new(store);
        assert!(matches!(
            evaluator.evaluate("WebSearch", &json!({"query": "rust"}), None),
            PolicyOutcome::AutoAllow { .. }
        ));
    }

    #[test]
    fn unknown_tool_deny_rule_with_content_still_fails_closed_without_a_target() {
        let store = PolicyStore::new();
        store.deny("WebSearch(rust)", PermissionRuleSource::ProjectSettings);
        let evaluator = PolicyEvaluator::new(store);
        assert!(matches!(
            evaluator.evaluate("WebSearch", &json!({"query": "anything"}), None),
            PolicyOutcome::AutoDeny { .. }
        ));
    }

    #[test]
    fn evaluator_auto_allows_exact_mcp_proxy_rule() {
        let store = PolicyStore::new();
        store.allow(
            "mcp__context-engine__search_context",
            PermissionRuleSource::ProjectSettings,
        );
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate(
            "mcp__context-engine__search_context",
            &json!({"query": "hello"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
        // A different tool on the same server is not covered by the
        // exact rule.
        let outcome = evaluator.evaluate(
            "mcp__context-engine__add_document",
            &json!({"text": "x"}),
            None,
        );
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }

    #[test]
    fn evaluator_auto_allows_mcp_server_scope_rule() {
        let store = PolicyStore::new();
        store.allow("mcp__context-engine", PermissionRuleSource::ProjectSettings);
        let evaluator = PolicyEvaluator::new(store);
        for tool in [
            "mcp__context-engine__search_context",
            "mcp__context-engine__add_document",
        ] {
            assert!(
                matches!(
                    evaluator.evaluate(tool, &json!({}), None),
                    PolicyOutcome::AutoAllow { .. }
                ),
                "{tool}"
            );
        }
        // A different server is not covered.
        assert_eq!(
            evaluator.evaluate("mcp__other__tool", &json!({}), None),
            PolicyOutcome::PassThrough
        );
    }

    #[test]
    fn evaluator_auto_allows_generic_mcp_facade_rule() {
        let store = PolicyStore::new();
        store.allow("Mcp(notes:*)", PermissionRuleSource::ProjectSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Mcp", &json!({"server": "notes", "name": "fetch"}), None);
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
        let outcome = evaluator.evaluate("Mcp", &json!({"server": "other", "name": "fetch"}), None);
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }

    #[test]
    fn evaluator_auto_allows_matching_bash_rule() {
        let store = PolicyStore::new();
        store.allow("Bash(ls:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Bash", &json!({"command": "ls -la"}), None);
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn evaluator_auto_allows_multiword_bash_prefix() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": "cargo test -p rebon-core"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));

        let outcome = evaluator.evaluate("Bash", &json!({"command": "cargo fmt"}), None);
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }

    #[test]
    fn evaluator_does_not_auto_allow_unsafe_compound_shell_commands() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);
        store.deny("Bash(rm:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        for command in [
            "cargo test ; rm -rf target",
            "cargo test || rm -rf target",
            "cargo test | tee out.log",
            "cargo test > out.log",
        ] {
            assert_eq!(
                evaluator.evaluate("Bash", &json!({"command": command}), None),
                PolicyOutcome::PassThrough
            );
        }

        assert!(matches!(
            evaluator.evaluate(
                "Bash",
                &json!({"command": "rm -rf target && cargo test"}),
                None
            ),
            PolicyOutcome::AutoDeny { .. }
        ));
    }

    #[test]
    fn exact_allow_rule_can_match_same_compound_command() {
        let store = PolicyStore::new();
        store.allow(
            "Bash(cargo test && rm -rf target)",
            PermissionRuleSource::UserSettings,
        );
        let evaluator = PolicyEvaluator::new(store);

        assert!(matches!(
            evaluator.evaluate(
                "Bash",
                &json!({"command": "cargo test && rm -rf target"}),
                None
            ),
            PolicyOutcome::AutoAllow { .. }
        ));
        assert_eq!(
            evaluator.evaluate("Bash", &json!({"command": "cargo test"}), None),
            PolicyOutcome::PassThrough
        );
    }

    #[test]
    fn evaluator_auto_allows_quoted_metacharacters_inside_arguments() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": "cargo test -- --exact 'name with && chars'"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn evaluator_auto_allows_safe_and_chain_with_same_prefix() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": "cargo test -p a && cargo test -p b"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn evaluator_auto_allows_safe_and_chain_with_multiple_allow_rules() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo fmt:*)", PermissionRuleSource::UserSettings);
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": "cargo fmt --all && cargo test -p a"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn evaluator_rejects_unsafe_or_uncovered_compound_shell_commands() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        for command in [
            "cargo test -p a && rm -rf target",
            "cargo test -p a ; cargo test -p b",
            "cargo test -p a || cargo test -p b",
            "cargo test -p a | sh",
            "cargo test -p a > out.log",
            "cargo test -p a &",
            "cargo test -p a &&",
            "cargo test $(echo foo)",
            "cargo testfoo",
            "cargo testify",
        ] {
            assert_eq!(
                evaluator.evaluate("Bash", &json!({"command": command}), None),
                PolicyOutcome::PassThrough,
                "{command}"
            );
        }
    }

    #[test]
    fn evaluator_deny_checks_segments_before_exact_allow() {
        let store = PolicyStore::new();
        store.allow(
            "Bash(cargo test -p a && cargo publish)",
            PermissionRuleSource::UserSettings,
        );
        store.deny("Bash(cargo publish:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": "cargo test -p a && cargo publish"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoDeny { .. }));
    }

    #[test]
    fn bash_prefix_rules_are_token_aware() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);

        for command in ["cargo testfoo", "cargo testify"] {
            assert_eq!(
                evaluator.evaluate("Bash", &json!({"command": command}), None),
                PolicyOutcome::PassThrough,
                "{command}"
            );
        }
    }

    #[test]
    fn policy_store_ignores_broad_shell_allow_rules() {
        let store = PolicyStore::new();
        store.allow("Bash", PermissionRuleSource::UserSettings);
        store.allow("Bash(*)", PermissionRuleSource::UserSettings);
        store.allow("Bash(bash:*)", PermissionRuleSource::UserSettings);
        store.allow("Bash(/bin/bash:*)", PermissionRuleSource::UserSettings);
        store.allow("Bash(/usr/bin/env:*)", PermissionRuleSource::UserSettings);
        store.allow("PowerShell", PermissionRuleSource::UserSettings);
        store.allow(
            "PowerShell(powershell:*)",
            PermissionRuleSource::UserSettings,
        );
        store.allow(
            r"PowerShell(C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe:*)",
            PermissionRuleSource::UserSettings,
        );
        store.allow(
            r#"PowerShell("C:\Program Files\PowerShell\7\pwsh.exe":*)"#,
            PermissionRuleSource::UserSettings,
        );
        store.allow("bash *", PermissionRuleSource::UserSettings);
        store.allow("powershell *", PermissionRuleSource::UserSettings);
        store.allow("Bash(cargo test:*)", PermissionRuleSource::UserSettings);

        assert_eq!(store.len(), 1);
        let evaluator = PolicyEvaluator::new(store);
        assert_eq!(
            evaluator.evaluate("Bash", &json!({"command": "bash -lc whoami"}), None),
            PolicyOutcome::PassThrough
        );
        assert!(matches!(
            evaluator.evaluate("Bash", &json!({"command": "cargo test"}), None),
            PolicyOutcome::AutoAllow { .. }
        ));
    }

    #[test]
    fn evaluator_deny_precedes_allow() {
        let store = PolicyStore::new();
        store.allow("Bash(rm:*)", PermissionRuleSource::UserSettings);
        store.deny("Bash(rm:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Bash", &json!({"command": "rm -rf /"}), None);
        assert!(matches!(outcome, PolicyOutcome::AutoDeny { .. }));
    }

    #[test]
    fn evaluator_passes_through_when_no_rule_matches() {
        let store = PolicyStore::new();
        store.allow("Bash(ls:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Bash", &json!({"command": "cat foo.txt"}), None);
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }

    #[test]
    fn evaluator_matches_aliased_tool_names() {
        let store = PolicyStore::new();
        store.allow("FileReadTool(/tmp/**)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Read", &json!({"file_path": "/tmp/foo.rs"}), None);
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    /// `Skill` is `rebon-plugin-skill`'s, so policy cannot ask the tool what
    /// it answers to; it reads the shared facts table instead. The table row
    /// is the only place `SkillTool` is written down, and this is what says
    /// so — a rule the user wrote against the older spelling has to keep
    /// covering the call.
    #[test]
    fn evaluator_resolves_the_skill_alias_through_the_shared_table() {
        assert!(rule_applies_to_tool(
            "SkillTool",
            rebon_tools_core::SKILL_TOOL_NAME
        ));
        let store = PolicyStore::new();
        store.deny("SkillTool", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate(
            rebon_tools_core::SKILL_TOOL_NAME,
            &json!({"skill": "commit"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoDeny { .. }));
    }

    #[test]
    fn policy_store_from_strings_builds_rule_set() {
        let store = PolicyStore::from_strings(
            ["Bash(ls:*)", "Edit(src/**)", "Read(/tmp/**)"],
            PermissionBehavior::Allow,
            PermissionRuleSource::UserSettings,
        );
        assert_eq!(store.len(), 3);
        let snapshot = store.snapshot();
        assert_eq!(snapshot[0].rule_value.tool_name, "Bash");
        assert_eq!(snapshot[0].rule_value.rule_content.as_deref(), Some("ls:*"));
    }

    // ── RulesBasedPermissionBroker end-to-end ─────────────────────────

    struct AskingTool;

    #[async_trait]
    impl Tool for AskingTool {
        fn id(&self) -> ToolId {
            ToolId::new("Bash")
        }
        fn description(&self) -> &str {
            "test bash"
        }
        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }
        async fn check_permissions(
            &self,
            input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::ask(
                PermissionRequest::new("Run bash", "needs approval"),
                Some(input.clone()),
            ))
        }
        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(input)
        }
    }

    /// Test broker that records every resolve() call + always allows.
    #[derive(Default)]
    struct RecordingDelegate {
        calls: Mutex<Vec<PermissionDecision>>,
    }

    impl RecordingDelegate {
        fn decisions(&self) -> Vec<PermissionDecision> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PermissionBroker for RecordingDelegate {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            input: Value,
            context: &ToolContext,
            decision: PermissionDecision,
        ) -> ToolResult<Value> {
            self.calls.lock().unwrap().push(decision.clone());
            // Always allow — we just want to see what the rules
            // broker passed in.
            let _ = decision;
            tool.call(input, context).await
        }
    }

    #[tokio::test]
    async fn rules_broker_auto_allows_and_rewrites_decision_before_delegate() {
        let store = PolicyStore::new();
        store.allow("Bash(ls:*)", PermissionRuleSource::UserSettings);

        let delegate = Arc::new(RecordingDelegate::default());
        let broker = RulesBasedPermissionBroker::new(store, delegate.clone());

        let tool = AskingTool;
        let ctx = ToolContext::new();
        let input = json!({ "command": "ls -la" });
        let decision = tool.check_permissions(&input, &ctx).await.unwrap();
        let out = broker
            .resolve(&tool, input.clone(), &ctx, decision)
            .await
            .unwrap();
        assert_eq!(out, input);

        // The delegate should have seen an Allow decision, not the
        // original Ask.
        let decisions = delegate.decisions();
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].behavior,
            rebon_tools_core::PermissionBehavior::Allow
        );
    }

    #[tokio::test]
    async fn rules_broker_auto_denies_when_deny_rule_matches() {
        let store = PolicyStore::new();
        store.deny("Bash(rm:*)", PermissionRuleSource::UserSettings);

        let delegate = Arc::new(RecordingDelegate::default());
        let broker = RulesBasedPermissionBroker::new(store, delegate.clone());

        let tool = AskingTool;
        let ctx = ToolContext::new();
        let input = json!({ "command": "rm -rf /" });
        let decision = tool.check_permissions(&input, &ctx).await.unwrap();
        let err = broker
            .resolve(&tool, input, &ctx, decision)
            .await
            .unwrap_err();
        match err {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(reason.contains("stored rule"));
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        // Delegate never saw the call.
        assert!(delegate.decisions().is_empty());
    }

    #[tokio::test]
    async fn rules_broker_passes_through_when_no_rule_matches() {
        let store = PolicyStore::new();
        store.allow("Bash(ls:*)", PermissionRuleSource::UserSettings);

        let delegate = Arc::new(DenyAskPermissionBroker);
        let broker = RulesBasedPermissionBroker::new(store, delegate);

        let tool = AskingTool;
        let ctx = ToolContext::new();
        let input = json!({ "command": "cat foo.txt" });
        let decision = tool.check_permissions(&input, &ctx).await.unwrap();
        let err = broker
            .resolve(&tool, input, &ctx, decision)
            .await
            .unwrap_err();
        // DenyAskPermissionBroker rejects Ask decisions — that
        // proves the rules broker passed the original Ask through.
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    #[tokio::test]
    async fn rules_broker_deny_takes_precedence_over_allow_in_store() {
        let store = PolicyStore::new();
        store.allow("Bash(rm:*)", PermissionRuleSource::UserSettings);
        store.deny("Bash(rm:*)", PermissionRuleSource::UserSettings);

        let delegate = Arc::new(RecordingDelegate::default());
        let broker = RulesBasedPermissionBroker::new(store, delegate.clone());

        let tool = AskingTool;
        let ctx = ToolContext::new();
        let input = json!({ "command": "rm -rf /" });
        let decision = tool.check_permissions(&input, &ctx).await.unwrap();
        let err = broker
            .resolve(&tool, input, &ctx, decision)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
        assert!(delegate.decisions().is_empty());
    }

    // ── Edit → Write alias ──────────────────────────────────────────

    #[test]
    fn edit_rule_also_matches_write_tool() {
        let store = PolicyStore::new();
        store.allow(
            "Edit(F:/dev/project/**)",
            PermissionRuleSource::UserSettings,
        );
        let evaluator = PolicyEvaluator::new(store);
        // Write call should match the Edit rule.
        let outcome = evaluator.evaluate(
            "Write",
            &json!({"file_path": "F:/dev/project/src/main.rs"}),
            None,
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn edit_rule_does_not_match_write_outside_path() {
        let store = PolicyStore::new();
        store.allow(
            "Edit(F:/dev/project/**)",
            PermissionRuleSource::UserSettings,
        );
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Write", &json!({"file_path": "F:/other/file.rs"}), None);
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }

    #[test]
    fn edit_rule_also_matches_multi_edit_and_notebook_edit() {
        let store = PolicyStore::new();
        store.allow(
            "Edit(F:/dev/project/**)",
            PermissionRuleSource::UserSettings,
        );
        let evaluator = PolicyEvaluator::new(store);

        let multi = evaluator.evaluate(
            "MultiEdit",
            &json!({
                "file_path": "F:/dev/project/src/main.rs",
                "edits": [{ "old_string": "a", "new_string": "b" }]
            }),
            None,
        );
        assert!(matches!(multi, PolicyOutcome::AutoAllow { .. }));

        // NotebookEdit names its target `notebook_path`; the rule must
        // still be evaluated against a real path, not an empty string.
        let notebook = evaluator.evaluate(
            "NotebookEdit",
            &json!({ "notebook_path": "F:/dev/project/analysis.ipynb", "new_source": "x" }),
            None,
        );
        assert!(matches!(notebook, PolicyOutcome::AutoAllow { .. }));
    }

    /// The half that matters: a deny rule written against `Edit` must not
    /// be escapable by reaching for a different file-edit tool.
    #[test]
    fn edit_deny_rule_covers_multi_edit_and_notebook_edit() {
        let store = PolicyStore::new();
        store.deny(
            "Edit(F:/dev/project/secrets/**)",
            PermissionRuleSource::UserSettings,
        );
        let evaluator = PolicyEvaluator::new(store);

        let multi = evaluator.evaluate(
            "MultiEdit",
            &json!({
                "file_path": "F:/dev/project/secrets/keys.toml",
                "edits": [{ "old_string": "a", "new_string": "b" }]
            }),
            None,
        );
        assert!(matches!(multi, PolicyOutcome::AutoDeny { .. }));

        let notebook = evaluator.evaluate(
            "NotebookEdit",
            &json!({ "notebook_path": "F:/dev/project/secrets/exfil.ipynb", "new_source": "x" }),
            None,
        );
        assert!(matches!(notebook, PolicyOutcome::AutoDeny { .. }));
    }

    /// A path-scoped deny must stay path-scoped: the umbrella widens which
    /// *tools* a rule covers, never which paths.
    #[test]
    fn edit_deny_rule_leaves_other_paths_alone_for_new_tools() {
        let store = PolicyStore::new();
        store.deny(
            "Edit(F:/dev/project/secrets/**)",
            PermissionRuleSource::UserSettings,
        );
        let evaluator = PolicyEvaluator::new(store);

        let multi = evaluator.evaluate(
            "MultiEdit",
            &json!({
                "file_path": "F:/dev/project/src/main.rs",
                "edits": [{ "old_string": "a", "new_string": "b" }]
            }),
            None,
        );
        assert_eq!(multi, PolicyOutcome::PassThrough);

        let notebook = evaluator.evaluate(
            "NotebookEdit",
            &json!({ "notebook_path": "F:/dev/project/src/analysis.ipynb", "new_source": "x" }),
            None,
        );
        assert_eq!(notebook, PolicyOutcome::PassThrough);
    }

    #[test]
    fn write_rule_still_matches_write_tool() {
        let store = PolicyStore::new();
        store.allow("Write(src/**)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Write", &json!({"file_path": "src/lib.rs"}), None);
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn edit_rule_does_not_match_read_tool() {
        let store = PolicyStore::new();
        store.allow("Edit(src/**)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate("Read", &json!({"file_path": "src/lib.rs"}), None);
        // Edit rules should NOT auto-allow Read — Read has its own
        // permission class.
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }

    // ── Bash cd-prefix stripping ────────────────────────────────────

    #[test]
    fn strip_cwd_prefix_removes_matching_cd() {
        assert_eq!(
            strip_cwd_prefix(r#"cd "F:\dev\project" && cargo test"#, "F:/dev/project"),
            Some("cargo test"),
        );
    }

    #[test]
    fn strip_cwd_prefix_normalizes_slashes() {
        assert_eq!(
            strip_cwd_prefix(
                r#"cd "C:\projects\example\workspace\app" && cargo test -p foo"#,
                "C:/projects/example/workspace/app",
            ),
            Some("cargo test -p foo"),
        );
    }

    #[test]
    fn strip_cwd_prefix_handles_single_quotes() {
        assert_eq!(
            strip_cwd_prefix(
                "cd '/home/user/project' && make build",
                "/home/user/project"
            ),
            Some("make build"),
        );
    }

    #[test]
    fn strip_cwd_prefix_handles_unquoted_path() {
        assert_eq!(
            strip_cwd_prefix("cd /home/user/project && ls -la", "/home/user/project"),
            Some("ls -la"),
        );
    }

    #[test]
    fn strip_cwd_prefix_returns_none_for_different_directory() {
        assert_eq!(
            strip_cwd_prefix("cd /tmp && rm -rf *", "/home/user/project"),
            None,
        );
    }

    #[test]
    fn strip_cwd_prefix_returns_none_without_cd() {
        assert_eq!(
            strip_cwd_prefix("cargo test -p foo", "/home/user/project"),
            None,
        );
    }

    #[test]
    fn evaluator_strips_cd_cwd_prefix_before_matching() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": r#"cd "F:\dev\project" && cargo test -p foo"#}),
            Some("F:/dev/project"),
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn evaluator_does_not_strip_cd_when_different_dir() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo:*)", PermissionRuleSource::UserSettings);
        // cd target differs from cwd — the `cd:*` fallback won't match
        // `cargo:*`, so this should pass through.
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": "cd /tmp && cargo test"}),
            Some("/home/user/project"),
        );
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }

    #[test]
    fn strip_powershell_cwd_prefix_removes_matching_set_location() {
        assert_eq!(
            strip_powershell_cwd_prefix(
                r#"Set-Location "F:\dev\project"; cargo test -p foo"#,
                "F:/dev/project",
            ),
            Some("cargo test -p foo"),
        );
        assert_eq!(
            strip_powershell_cwd_prefix(
                r#"set-location -LiteralPath 'F:\dev\project'; cargo check"#,
                "F:/dev/project",
            ),
            Some("cargo check"),
        );
        assert_eq!(
            strip_powershell_cwd_prefix(
                r#"cd -Path 'F:\dev\project'; cargo fmt --check"#,
                "F:/dev/project",
            ),
            Some("cargo fmt --check"),
        );
    }

    #[test]
    fn strip_powershell_cwd_prefix_handles_separator_inside_quoted_path() {
        assert_eq!(
            strip_powershell_cwd_prefix(
                r#"Set-Location 'F:\dev\project;copy'; cargo test"#,
                "F:/dev/project;copy",
            ),
            Some("cargo test"),
        );
    }

    #[test]
    fn strip_powershell_cwd_prefix_rejects_other_or_dynamic_locations() {
        assert_eq!(
            strip_powershell_cwd_prefix(r#"Set-Location "F:\other"; cargo test"#, "F:/dev/project",),
            None,
        );
        assert_eq!(
            strip_powershell_cwd_prefix("Set-Location $dest; cargo test", "F:/dev/project"),
            None,
        );
    }

    #[test]
    fn evaluator_strips_powershell_set_location_before_matching() {
        let store = PolicyStore::new();
        store.allow("PowerShell(cargo:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate(
            "PowerShell",
            &json!({
                "command": r#"Set-Location "F:\dev\project"; cargo test -p foo"#
            }),
            Some("F:/dev/project"),
        );
        assert!(matches!(outcome, PolicyOutcome::AutoAllow { .. }));
    }

    #[test]
    fn evaluator_keeps_powershell_tail_subject_to_deny_rules() {
        let store = PolicyStore::new();
        store.deny("PowerShell(git:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        let outcome = evaluator.evaluate(
            "PowerShell",
            &json!({
                "command": r#"Set-Location "F:\dev\project"; git restore src/lib.rs"#
            }),
            Some("F:/dev/project"),
        );
        assert!(matches!(outcome, PolicyOutcome::AutoDeny { .. }));
    }

    #[test]
    fn evaluator_no_cwd_skips_normalization() {
        let store = PolicyStore::new();
        store.allow("Bash(cargo:*)", PermissionRuleSource::UserSettings);
        let evaluator = PolicyEvaluator::new(store);
        // Without cwd, the cd prefix is NOT stripped.
        let outcome = evaluator.evaluate(
            "Bash",
            &json!({"command": "cd /project && cargo test"}),
            None,
        );
        assert_eq!(outcome, PolicyOutcome::PassThrough);
    }
}
