use std::collections::HashMap;
use std::fmt;

// The wire half — method, capability key, meta-key rule, params shape — is
// shared with `rebon mcp serve`, which writes what this host reads.
pub use rebon_proto::mcp_channel::{
    is_safe_meta_key, ChannelMessage, CHANNEL_CAPABILITY, CHANNEL_NOTIFICATION_METHOD,
    CHANNEL_PERMISSION_CAPABILITY, CHANNEL_PERMISSION_METHOD, CHANNEL_PERMISSION_REQUEST_METHOD,
};

pub const CHANNEL_TAG: &str = "channel";
pub const ID_ALPHABET: &str = "abcdefghijkmnopqrstuvwxyz";
pub const MAX_PREVIEW_CHARS: usize = 200;

const ID_AVOID_SUBSTRINGS: &[&str] = &[
    "fuck", "shit", "cunt", "cock", "dick", "twat", "piss", "crap", "bitch", "whore", "ass", "tit",
    "cum", "fag", "dyke", "nig", "kike", "rape", "nazi", "damn", "poo", "pee", "wank", "anus",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEntry {
    Plugin {
        name: String,
        marketplace: String,
        dev: bool,
    },
    Server {
        name: String,
        dev: bool,
    },
}

impl ChannelEntry {
    pub fn with_dev(mut self, dev: bool) -> Self {
        match &mut self {
            ChannelEntry::Plugin { dev: d, .. } | ChannelEntry::Server { dev: d, .. } => *d = dev,
        }
        self
    }

    pub fn dev(&self) -> bool {
        match self {
            ChannelEntry::Plugin { dev, .. } | ChannelEntry::Server { dev, .. } => *dev,
        }
    }
}

impl fmt::Display for ChannelEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelEntry::Plugin {
                name, marketplace, ..
            } => write!(f, "plugin:{name}@{marketplace}"),
            ChannelEntry::Server { name, .. } => write!(f, "server:{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelEntryParseError {
    pub value: String,
    pub message: String,
}

impl fmt::Display for ChannelEntryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid channel entry `{}`: {}",
            self.value, self.message
        )
    }
}

impl std::error::Error for ChannelEntryParseError {}

pub fn parse_channel_entry(raw: &str) -> Result<ChannelEntry, ChannelEntryParseError> {
    if let Some(rest) = raw.strip_prefix("plugin:") {
        let (name, marketplace) = rest.split_once('@').ok_or_else(|| {
            parse_error(raw, "plugin entries must be plugin:<name>@<marketplace>")
        })?;
        if name.is_empty() || marketplace.is_empty() || marketplace.contains('@') {
            return Err(parse_error(
                raw,
                "plugin entries must include exactly one non-empty name and marketplace",
            ));
        }
        return Ok(ChannelEntry::Plugin {
            name: name.to_string(),
            marketplace: marketplace.to_string(),
            dev: false,
        });
    }
    if let Some(name) = raw.strip_prefix("server:") {
        if name.is_empty() {
            return Err(parse_error(raw, "server entries must be server:<name>"));
        }
        return Ok(ChannelEntry::Server {
            name: name.to_string(),
            dev: false,
        });
    }
    Err(parse_error(
        raw,
        "expected plugin:<name>@<marketplace> or server:<name>",
    ))
}

fn parse_error(raw: &str, message: &str) -> ChannelEntryParseError {
    ChannelEntryParseError {
        value: raw.to_string(),
        message: message.to_string(),
    }
}

pub fn parse_channel_entries<I, S>(entries: I) -> Result<Vec<ChannelEntry>, ChannelEntryParseError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    entries
        .into_iter()
        .map(|entry| parse_channel_entry(entry.as_ref()))
        .collect()
}

pub fn wrap_channel_message(
    server_name: &str,
    content: &str,
    meta: Option<&HashMap<String, String>>,
) -> String {
    let mut attrs = String::new();
    if let Some(meta) = meta {
        let mut pairs: Vec<_> = meta.iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in pairs {
            if is_safe_meta_key(key) {
                attrs.push(' ');
                attrs.push_str(key);
                attrs.push_str("=\"");
                attrs.push_str(&escape_xml_attr(value));
                attrs.push('"');
            }
        }
    }
    format!(
        "<{CHANNEL_TAG} source=\"{}\"{}>\n{}\n</{CHANNEL_TAG}>",
        escape_xml_attr(server_name),
        attrs,
        content
    )
}

pub fn escape_xml_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAllowlistEntry {
    pub marketplace: String,
    pub plugin: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionType {
    Individual,
    Team,
    Enterprise,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveAllowlistSource {
    Org,
    Ledger,
}

pub fn effective_channel_allowlist(
    subscription: SubscriptionType,
    org_list: Option<Vec<ChannelAllowlistEntry>>,
    ledger: Vec<ChannelAllowlistEntry>,
) -> (Vec<ChannelAllowlistEntry>, EffectiveAllowlistSource) {
    if matches!(
        subscription,
        SubscriptionType::Team | SubscriptionType::Enterprise
    ) {
        if let Some(org) = org_list {
            return (org, EffectiveAllowlistSource::Org);
        }
    }
    (ledger, EffectiveAllowlistSource::Ledger)
}

pub fn find_channel_entry<'a>(
    server_name: &str,
    entries: &'a [ChannelEntry],
) -> Option<&'a ChannelEntry> {
    let parts: Vec<&str> = server_name.split(':').collect();
    entries.iter().find(|entry| match entry {
        ChannelEntry::Server { name, .. } => server_name == name,
        ChannelEntry::Plugin { name, .. } => {
            parts.first() == Some(&"plugin") && parts.get(1) == Some(&name.as_str())
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSkipKind {
    Capability,
    Disabled,
    Auth,
    Policy,
    Session,
    Marketplace,
    Allowlist,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelGateResult {
    Register,
    Skip {
        kind: ChannelSkipKind,
        reason: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ChannelCapabilities {
    pub claude_channel: bool,
}

#[derive(Debug, Clone)]
pub struct ChannelGateContext {
    pub runtime_enabled: bool,
    pub has_oauth: bool,
    pub subscription: SubscriptionType,
    pub org_channels_enabled: Option<bool>,
    pub org_allowlist: Option<Vec<ChannelAllowlistEntry>>,
    pub ledger_allowlist: Vec<ChannelAllowlistEntry>,
    pub session_entries: Vec<ChannelEntry>,
}

pub fn gate_channel_server(
    server_name: &str,
    capabilities: &ChannelCapabilities,
    plugin_source: Option<&str>,
    ctx: &ChannelGateContext,
) -> ChannelGateResult {
    if !capabilities.claude_channel {
        return skip(
            ChannelSkipKind::Capability,
            "server did not declare claude/channel capability",
        );
    }
    if !ctx.runtime_enabled {
        return skip(
            ChannelSkipKind::Disabled,
            "channels feature is not currently available",
        );
    }
    if !ctx.has_oauth {
        return skip(
            ChannelSkipKind::Auth,
            "channels requires an authenticated session (run /login)",
        );
    }
    let managed = matches!(
        ctx.subscription,
        SubscriptionType::Team | SubscriptionType::Enterprise
    );
    if managed && ctx.org_channels_enabled != Some(true) {
        return skip(
            ChannelSkipKind::Policy,
            "channels not enabled by org policy (set channelsEnabled: true in managed settings)",
        );
    }
    let Some(entry) = find_channel_entry(server_name, &ctx.session_entries) else {
        return skip(
            ChannelSkipKind::Session,
            format!("server {server_name} not in --channels list for this session"),
        );
    };
    match entry {
        ChannelEntry::Plugin {
            name,
            marketplace,
            dev,
        } => {
            let actual = plugin_source.and_then(parse_plugin_marketplace);
            if actual.as_deref() != Some(marketplace.as_str()) {
                return skip(ChannelSkipKind::Marketplace, format!("you asked for plugin:{name}@{marketplace} but the installed {name} plugin is from {}", actual.unwrap_or_else(|| "an unknown source".to_string())));
            }
            if !dev {
                let (entries, source) = effective_channel_allowlist(
                    ctx.subscription,
                    ctx.org_allowlist.clone(),
                    ctx.ledger_allowlist.clone(),
                );
                if !entries
                    .iter()
                    .any(|e| e.plugin == *name && e.marketplace == *marketplace)
                {
                    let reason = match source {
                        EffectiveAllowlistSource::Org => format!("plugin {name}@{marketplace} is not on your org's approved channels list (set allowedChannelPlugins in managed settings)"),
                        EffectiveAllowlistSource::Ledger => format!("plugin {name}@{marketplace} is not on the approved channels allowlist (use --dangerously-load-development-channels for local dev)"),
                    };
                    return skip(ChannelSkipKind::Allowlist, reason);
                }
            }
        }
        ChannelEntry::Server { name, dev } => {
            if !dev {
                return skip(ChannelSkipKind::Allowlist, format!("server {name} is not on the approved channels allowlist (use --dangerously-load-development-channels for local dev)"));
            }
        }
    }
    ChannelGateResult::Register
}

fn skip(kind: ChannelSkipKind, reason: impl Into<String>) -> ChannelGateResult {
    ChannelGateResult::Skip {
        kind,
        reason: reason.into(),
    }
}

pub fn parse_plugin_marketplace(source: &str) -> Option<String> {
    let (_name, marketplace) = source.rsplit_once('@')?;
    if marketplace.is_empty() {
        None
    } else {
        Some(marketplace.to_string())
    }
}

/// Allow / deny / ask, defined once for the whole tree.
///
/// A channel reply only ever carries allow or deny — a chat message cannot
/// re-ask the question it is answering — but it is the same decision the rest
/// of the permission pipeline acts on, so it is the same type.
pub use rebon_tools_core::PermissionBehavior;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPermissionReply {
    pub behavior: PermissionBehavior,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPermissionRequestParams {
    pub request_id: String,
    pub tool_name: String,
    pub description: String,
    pub input_preview: String,
}

pub fn parse_permission_reply(raw: &str) -> Option<ParsedPermissionReply> {
    let mut parts = raw.trim().split_whitespace();
    let behavior_raw = parts.next()?;
    let request_id = parts.next()?;
    if parts.next().is_some()
        || request_id.len() != 5
        || !request_id
            .chars()
            .all(|c| ID_ALPHABET.contains(c.to_ascii_lowercase()))
    {
        return None;
    }
    let behavior = match behavior_raw.to_ascii_lowercase().as_str() {
        "y" | "yes" => PermissionBehavior::Allow,
        "n" | "no" => PermissionBehavior::Deny,
        _ => return None,
    };
    Some(ParsedPermissionReply {
        behavior,
        request_id: request_id.to_ascii_lowercase(),
    })
}

pub fn short_request_id(tool_use_id: &str) -> String {
    let mut candidate = hash_to_id(tool_use_id);
    for salt in 0..10 {
        if !ID_AVOID_SUBSTRINGS
            .iter()
            .any(|bad| candidate.contains(bad))
        {
            return candidate;
        }
        candidate = hash_to_id(&format!("{tool_use_id}:{salt}"));
    }
    candidate
}

fn hash_to_id(input: &str) -> String {
    let mut h: u32 = 0x811c9dc5;
    for unit in input.encode_utf16() {
        h ^= u32::from(unit);
        h = h.wrapping_mul(0x01000193);
    }
    let alphabet = ID_ALPHABET.as_bytes();
    let mut s = String::with_capacity(5);
    for _ in 0..5 {
        s.push(alphabet[(h % 25) as usize] as char);
        h /= 25;
    }
    s
}

pub fn truncate_for_preview(input: &serde_json::Value) -> String {
    match serde_json::to_string(input) {
        Ok(s) if s.chars().count() > MAX_PREVIEW_CHARS => {
            let truncated: String = s.chars().take(MAX_PREVIEW_CHARS).collect();
            format!("{truncated}…")
        }
        Ok(s) => s,
        Err(_) => "(unserializable)".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPermissionResponse {
    pub behavior: PermissionBehavior,
    pub from_server: String,
}

#[derive(Default)]
pub struct ChannelPermissionCallbacks {
    pending: HashMap<String, Box<dyn FnMut(ChannelPermissionResponse) + Send>>,
}

impl ChannelPermissionCallbacks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_response<F>(&mut self, request_id: &str, handler: F)
    where
        F: FnMut(ChannelPermissionResponse) + Send + 'static,
    {
        self.pending
            .insert(request_id.to_ascii_lowercase(), Box::new(handler));
    }

    pub fn unsubscribe(&mut self, request_id: &str) -> bool {
        self.pending
            .remove(&request_id.to_ascii_lowercase())
            .is_some()
    }

    pub fn resolve(
        &mut self,
        request_id: &str,
        behavior: PermissionBehavior,
        from_server: &str,
    ) -> bool {
        let key = request_id.to_ascii_lowercase();
        let Some(mut resolver) = self.pending.remove(&key) else {
            return false;
        };
        resolver(ChannelPermissionResponse {
            behavior,
            from_server: from_server.to_string(),
        });
        true
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn plugin(name: &str, market: &str, dev: bool) -> ChannelEntry {
        ChannelEntry::Plugin {
            name: name.into(),
            marketplace: market.into(),
            dev,
        }
    }

    fn server(name: &str, dev: bool) -> ChannelEntry {
        ChannelEntry::Server {
            name: name.into(),
            dev,
        }
    }

    fn base_ctx(entries: Vec<ChannelEntry>) -> ChannelGateContext {
        ChannelGateContext {
            runtime_enabled: true,
            has_oauth: true,
            subscription: SubscriptionType::Individual,
            org_channels_enabled: None,
            org_allowlist: None,
            ledger_allowlist: vec![ChannelAllowlistEntry {
                marketplace: "official".into(),
                plugin: "slack".into(),
            }],
            session_entries: entries,
        }
    }

    #[test]
    fn parses_valid_entries_and_formats_without_dev() {
        let p = parse_channel_entry("plugin:slack@official").unwrap();
        let s = parse_channel_entry("server:local").unwrap();
        assert_eq!(p, plugin("slack", "official", false));
        assert_eq!(s, server("local", false));
        assert_eq!(p.to_string(), "plugin:slack@official");
        assert_eq!(s.to_string(), "server:local");
    }

    #[test]
    fn rejects_invalid_entries_with_value() {
        let err = parse_channel_entry("plugin:slack").unwrap_err();
        assert_eq!(err.value, "plugin:slack");
        assert!(parse_channel_entry("server:").is_err());
        assert!(parse_channel_entry("slack").is_err());
    }

    #[test]
    fn wraps_xml_escaping_attrs_filtering_keys_and_preserving_content() {
        let mut meta = HashMap::new();
        meta.insert("user".into(), "A&B\"<>'".into());
        meta.insert("bad-key".into(), "drop".into());
        let wrapped = wrap_channel_message("plugin:slack:bot", "<raw>&content", Some(&meta));
        assert_eq!(wrapped, "<channel source=\"plugin:slack:bot\" user=\"A&amp;B&quot;&lt;&gt;&apos;\">\n<raw>&content\n</channel>");
    }

    #[test]
    fn finds_server_exact_and_plugin_second_segment() {
        let entries = vec![server("plain", false), plugin("slack", "official", false)];
        assert_eq!(find_channel_entry("plain", &entries), Some(&entries[0]));
        assert_eq!(
            find_channel_entry("plugin:slack:bot", &entries),
            Some(&entries[1])
        );
        assert_eq!(find_channel_entry("plugin:other:bot", &entries), None);
    }

    #[test]
    fn gate_skip_order_and_register_matrix() {
        let caps = ChannelCapabilities {
            claude_channel: true,
        };
        let no_caps = ChannelCapabilities {
            claude_channel: false,
        };
        assert!(matches!(
            gate_channel_server("x", &no_caps, None, &base_ctx(vec![])),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Capability,
                ..
            }
        ));

        let mut ctx = base_ctx(vec![plugin("slack", "official", false)]);
        ctx.runtime_enabled = false;
        assert!(matches!(
            gate_channel_server("plugin:slack:bot", &caps, Some("slack@official"), &ctx),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Disabled,
                ..
            }
        ));
        ctx.runtime_enabled = true;
        ctx.has_oauth = false;
        assert!(matches!(
            gate_channel_server("plugin:slack:bot", &caps, Some("slack@official"), &ctx),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Auth,
                ..
            }
        ));

        let mut ctx = base_ctx(vec![plugin("slack", "official", false)]);
        ctx.subscription = SubscriptionType::Team;
        ctx.org_channels_enabled = Some(false);
        assert!(matches!(
            gate_channel_server("plugin:slack:bot", &caps, Some("slack@official"), &ctx),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Policy,
                ..
            }
        ));

        assert!(matches!(
            gate_channel_server(
                "plugin:slack:bot",
                &caps,
                Some("slack@official"),
                &base_ctx(vec![])
            ),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Session,
                ..
            }
        ));
        assert!(matches!(
            gate_channel_server(
                "plugin:slack:bot",
                &caps,
                Some("slack@evil"),
                &base_ctx(vec![plugin("slack", "official", false)])
            ),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Marketplace,
                ..
            }
        ));
        assert!(matches!(
            gate_channel_server(
                "plain",
                &caps,
                None,
                &base_ctx(vec![server("plain", false)])
            ),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Allowlist,
                ..
            }
        ));
        assert_eq!(
            gate_channel_server("plain", &caps, None, &base_ctx(vec![server("plain", true)])),
            ChannelGateResult::Register
        );
        assert!(matches!(
            gate_channel_server(
                "plugin:other:bot",
                &caps,
                Some("other@official"),
                &base_ctx(vec![plugin("other", "official", false)])
            ),
            ChannelGateResult::Skip {
                kind: ChannelSkipKind::Allowlist,
                ..
            }
        ));
        assert_eq!(
            gate_channel_server(
                "plugin:other:bot",
                &caps,
                Some("other@official"),
                &base_ctx(vec![plugin("other", "official", true)])
            ),
            ChannelGateResult::Register
        );
        assert_eq!(
            gate_channel_server(
                "plugin:slack:bot",
                &caps,
                Some("slack@official"),
                &base_ctx(vec![plugin("slack", "official", false)])
            ),
            ChannelGateResult::Register
        );
    }

    #[test]
    fn org_allowlist_replaces_ledger_for_managed_orgs() {
        let caps = ChannelCapabilities {
            claude_channel: true,
        };
        let mut ctx = base_ctx(vec![plugin("slack", "official", false)]);
        ctx.subscription = SubscriptionType::Enterprise;
        ctx.org_channels_enabled = Some(true);
        ctx.org_allowlist = Some(vec![]);
        assert!(
            matches!(gate_channel_server("plugin:slack:bot", &caps, Some("slack@official"), &ctx), ChannelGateResult::Skip { kind: ChannelSkipKind::Allowlist, reason } if reason.contains("org"))
        );
    }

    #[test]
    fn parses_permission_replies_exactly() {
        assert_eq!(
            parse_permission_reply(" yes ABCDE ").unwrap(),
            ParsedPermissionReply {
                behavior: PermissionBehavior::Allow,
                request_id: "abcde".into()
            }
        );
        assert_eq!(
            parse_permission_reply("n kmnoz").unwrap().behavior,
            PermissionBehavior::Deny
        );
        assert!(parse_permission_reply("yes abcde trailing").is_none());
        assert!(parse_permission_reply("yes ablde").is_none());
        assert!(parse_permission_reply("yes 12345").is_none());
    }

    /// A channel reply is an outward contract; the words it maps onto must
    /// stay the ones the rest of the permission pipeline serializes.
    #[test]
    fn channel_replies_carry_the_shared_wire_words() {
        assert_eq!(
            parse_permission_reply("y abcde")
                .unwrap()
                .behavior
                .as_wire(),
            "allow"
        );
        assert_eq!(
            parse_permission_reply("no abcde")
                .unwrap()
                .behavior
                .as_wire(),
            "deny"
        );
    }

    #[test]
    fn short_id_is_stable_uses_alphabet_and_avoids_blocklist() {
        let a = short_request_id("toolu_example_123");
        assert_eq!(a, short_request_id("toolu_example_123"));
        assert_eq!(a.len(), 5);
        assert!(a.chars().all(|c| ID_ALPHABET.contains(c)));
        assert!(!ID_AVOID_SUBSTRINGS.iter().any(|bad| a.contains(bad)));
    }

    #[test]
    fn preview_truncates_json() {
        assert_eq!(
            truncate_for_preview(&serde_json::json!({"a":1})),
            "{\"a\":1}"
        );
        let long = serde_json::Value::String("x".repeat(250));
        let preview = truncate_for_preview(&long);
        assert_eq!(preview.chars().count(), 201);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn callbacks_delete_before_resolve_and_reject_duplicate_or_stale() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_handler = Arc::clone(&seen);
        let mut callbacks = ChannelPermissionCallbacks::new();
        callbacks.on_response("AbCdE", move |response| {
            seen_for_handler.lock().unwrap().push(response);
        });
        assert_eq!(callbacks.pending_len(), 1);
        assert!(callbacks.resolve("abcde", PermissionBehavior::Allow, "server1"));
        assert_eq!(callbacks.pending_len(), 0);
        assert!(!callbacks.resolve("abcde", PermissionBehavior::Deny, "server1"));
        assert!(!callbacks.resolve("zzzzz", PermissionBehavior::Deny, "server1"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].behavior, PermissionBehavior::Allow);
    }
}
