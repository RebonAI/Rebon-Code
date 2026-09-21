//! Turning a controller's permission decision into one of Rebon's permission
//! options.
//!
//! A controller answers a `permission_request` with the decision shape
//! [`crate::permission_callbacks`] already models — `behavior` (`allow` /
//! `deny`), optional `updated_input`, optional `updated_permissions`, optional
//! `message`. A Rebon session takes none of that directly: its permission
//! prompt offers a fixed set of option ids and the runner answers by picking
//! one. The mapping between the two shapes is pure logic and lives here.
//! Rebon's option ids are mirrored in a small local enum
//! ([`RebonPermissionOption`]) rather than imported, because this crate must
//! not depend on any `rebon-*` crate.
//!
//! ## Remote answers are one-shot by default
//!
//! A decision made on a phone about one tool call should not quietly become a
//! standing rule on the machine. Under the default
//! [`RemotePermissionPolicy::one_shot`] policy a decision carrying
//! `updated_permissions` — the controller asking for "always" — is
//! **refused**, not downgraded: answering "once" to a request for "always"
//! would tell the controller something that did not happen. A runner that
//! wants remote "always" opts in with
//! [`RemotePermissionPolicy::allow_persistent_rules`].
//!
//! The same goes for `updated_input`, the controller having edited the tool
//! call before approving it. Approving the *original* input would run
//! something the user did not approve, so an edited input is refused unless
//! the runner opts in with [`RemotePermissionPolicy::allow_updated_input`] —
//! and then the answer carries the edited input, which the runner must
//! apply.
//!
//! | `behavior` | `updated_permissions` | `updated_input` | policy | result |
//! |---|---|---|---|---|
//! | `allow` | — | — | any | `allow_once` |
//! | `allow` | present | — | one-shot | refused: [`RemotePermissionRefusal::PersistentRuleNotAllowed`] |
//! | `allow` | present | — | persistent rules allowed | `allow_always` |
//! | `allow` | any | present | edited input not allowed | refused: [`RemotePermissionRefusal::UpdatedInputNotAllowed`] (after the rule check) |
//! | `allow` | any | present | edited input allowed | as above, carrying the input |
//! | `deny` | — | — | any | `reject_once` |
//! | `deny` | present | — | one-shot | refused: [`RemotePermissionRefusal::PersistentRuleNotAllowed`] |
//! | `deny` | present | — | persistent rules allowed | refused: [`RemotePermissionRefusal::PersistentDenyUnsupported`] — Rebon has no "reject always" |
//! | `deny` | any | present | any | refused: [`RemotePermissionRefusal::UpdatedInputOnDeny`] |
//!
//! `allow_always_generalized` is never chosen. The rules in
//! `updated_permissions` are opaque here, so this crate cannot tell an
//! exact rule from a generalized one, and picking the broader option
//! for someone who did not ask for it would be the wrong way to guess.
//!
//! A refusal is an answer to the *controller*; the prompt stays pending
//! and the controller can send a plain decision instead.

use std::fmt;

use serde_json::Value;

use crate::permission_callbacks::{
    parse_behavior, BridgePermissionBehavior, BridgePermissionResponse, OpaqueJson,
};

/// Rebon's permission option ids, mirrored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RebonPermissionOption {
    /// `allow_once` — allow this one call.
    AllowOnce,
    /// `allow_always` — allow, and remember an exact rule.
    AllowAlways,
    /// `allow_always_generalized` — allow, and remember a broader rule.
    AllowAlwaysGeneralized,
    /// `reject_once` — reject this one call.
    RejectOnce,
}

impl RebonPermissionOption {
    /// Every option, in Rebon's display order.
    pub const ALL: [Self; 4] = [
        Self::AllowOnce,
        Self::AllowAlways,
        Self::AllowAlwaysGeneralized,
        Self::RejectOnce,
    ];

    /// Rebon's option id.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowAlways => "allow_always",
            Self::AllowAlwaysGeneralized => "allow_always_generalized",
            Self::RejectOnce => "reject_once",
        }
    }

    /// Parse one of Rebon's option ids.
    pub fn parse(option_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|option| option.as_str() == option_id)
    }

    /// Whether the option lets the call run.
    pub fn allows(self) -> bool {
        !matches!(self, Self::RejectOnce)
    }

    /// Whether the option leaves a rule behind.
    pub fn persists(self) -> bool {
        matches!(self, Self::AllowAlways | Self::AllowAlwaysGeneralized)
    }
}

impl fmt::Display for RebonPermissionOption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a runner lets a remote decision do.
///
/// The default is [`Self::one_shot`]: no standing rules, no edited
/// input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemotePermissionPolicy {
    /// Let a decision with `updated_permissions` become `allow_always`.
    pub allow_persistent_rules: bool,
    /// Let an `allow` carry an edited `updated_input`.
    pub allow_updated_input: bool,
}

impl RemotePermissionPolicy {
    /// Remote answers affect this one call only, as asked.
    pub const fn one_shot() -> Self {
        Self {
            allow_persistent_rules: false,
            allow_updated_input: false,
        }
    }

    /// Also let a remote "always" leave a rule behind.
    pub const fn allowing_persistent_rules(mut self) -> Self {
        self.allow_persistent_rules = true;
        self
    }

    /// Also let a remote approval carry an edited tool input.
    pub const fn allowing_updated_input(mut self) -> Self {
        self.allow_updated_input = true;
        self
    }
}

/// The option a runner should answer the prompt with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePermissionAnswer {
    /// The Rebon option to select.
    pub option: RebonPermissionOption,
    /// The edited tool input the runner must run instead of the
    /// original. Only ever present when the policy allows it.
    pub updated_input: Option<OpaqueJson>,
    /// The controller's message, passed through.
    pub message: Option<String>,
}

/// Why a decision cannot be applied as sent. The prompt stays pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePermissionRefusal {
    /// The decision asks for a standing rule and the policy is one-shot.
    PersistentRuleNotAllowed,
    /// The decision asks to deny *always*, which Rebon cannot express.
    PersistentDenyUnsupported,
    /// The approval edits the tool input and the policy does not allow
    /// that.
    UpdatedInputNotAllowed,
    /// A denial carries an edited input, which means nothing.
    UpdatedInputOnDeny,
}

impl fmt::Display for RemotePermissionRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PersistentRuleNotAllowed => {
                "remote decisions apply to this call only; send it without updated_permissions"
            }
            Self::PersistentDenyUnsupported => {
                "Rebon cannot remember a denial; send it without updated_permissions"
            }
            Self::UpdatedInputNotAllowed => {
                "this machine does not accept an edited tool input from a remote decision"
            }
            Self::UpdatedInputOnDeny => "a denial cannot carry updated_input",
        })
    }
}

impl std::error::Error for RemotePermissionRefusal {}

/// Map a controller's decision to the Rebon option to answer with, under
/// `policy`. See the module docs for the table.
pub fn map_remote_decision(
    decision: &BridgePermissionResponse,
    policy: RemotePermissionPolicy,
) -> Result<RemotePermissionAnswer, RemotePermissionRefusal> {
    let allow = decision.behavior == BridgePermissionBehavior::Allow;
    if !allow && decision.updated_input.is_some() {
        return Err(RemotePermissionRefusal::UpdatedInputOnDeny);
    }
    let persistent = decision.updated_permissions.is_some();
    if persistent {
        if !policy.allow_persistent_rules {
            return Err(RemotePermissionRefusal::PersistentRuleNotAllowed);
        }
        if !allow {
            return Err(RemotePermissionRefusal::PersistentDenyUnsupported);
        }
    }
    if decision.updated_input.is_some() && !policy.allow_updated_input {
        return Err(RemotePermissionRefusal::UpdatedInputNotAllowed);
    }
    let option = match (allow, persistent) {
        (true, false) => RebonPermissionOption::AllowOnce,
        (true, true) => RebonPermissionOption::AllowAlways,
        (false, _) => RebonPermissionOption::RejectOnce,
    };
    Ok(RemotePermissionAnswer {
        option,
        updated_input: decision.updated_input.clone(),
        message: decision.message.clone(),
    })
}

/// Why a decision's JSON could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDecisionParseError {
    /// The decision is not a JSON object.
    NotAnObject,
    /// There is no `behavior` string.
    MissingBehavior,
    /// `behavior` is neither `allow` nor `deny`.
    UnknownBehavior(String),
    /// A field has the wrong type.
    BadField(&'static str),
    /// A field is present under both its snake_case and camelCase names.
    ConflictingSpellings(&'static str),
}

impl fmt::Display for RemoteDecisionParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => formatter.write_str("a permission decision must be an object"),
            Self::MissingBehavior => formatter.write_str("a permission decision needs a behavior"),
            Self::UnknownBehavior(behavior) => {
                write!(formatter, "unknown permission behavior {behavior:?}")
            }
            Self::BadField(field) => write!(formatter, "`{field}` has the wrong type"),
            Self::ConflictingSpellings(field) => {
                write!(formatter, "`{field}` is given under two spellings")
            }
        }
    }
}

impl std::error::Error for RemoteDecisionParseError {}

/// Take a field that may be spelt snake_case or camelCase — both
/// spellings are in use for this shape. Silently ignoring the one we did
/// not expect would turn an "always" into a "once", so both are read and
/// giving both is an error. `null` counts as absent.
fn take_either<'a>(
    object: &'a serde_json::Map<String, Value>,
    snake: &'static str,
    camel: &'static str,
) -> Result<Option<&'a Value>, RemoteDecisionParseError> {
    let present = |key: &str| object.get(key).filter(|value| !value.is_null());
    match (present(snake), present(camel)) {
        (Some(_), Some(_)) => Err(RemoteDecisionParseError::ConflictingSpellings(snake)),
        (value, None) | (None, value) => Ok(value),
    }
}

/// Read the `response` object of a `permission_response` frame
/// ([`crate::config::PermissionResponseBody::response`]).
///
/// `updated_input` is kept as long as it is not `null`;
/// `updated_permissions` must be an array, and an empty one counts as
/// absent (no rules to install). Keys this shape does not define are
/// ignored.
pub fn parse_remote_decision(
    value: &Value,
) -> Result<BridgePermissionResponse, RemoteDecisionParseError> {
    let object = value
        .as_object()
        .ok_or(RemoteDecisionParseError::NotAnObject)?;
    let behavior = match object.get("behavior") {
        Some(Value::String(behavior)) => parse_behavior(behavior)
            .ok_or_else(|| RemoteDecisionParseError::UnknownBehavior(behavior.clone()))?,
        Some(_) => return Err(RemoteDecisionParseError::BadField("behavior")),
        None => return Err(RemoteDecisionParseError::MissingBehavior),
    };
    let mut decision = BridgePermissionResponse::new(behavior);
    if let Some(input) = take_either(object, "updated_input", "updatedInput")? {
        decision.updated_input = Some(OpaqueJson::new(input.to_string()));
    }
    match take_either(object, "updated_permissions", "updatedPermissions")? {
        None => {}
        Some(Value::Array(rules)) if rules.is_empty() => {}
        Some(rules @ Value::Array(_)) => {
            decision.updated_permissions = Some(OpaqueJson::new(rules.to_string()));
        }
        Some(_) => return Err(RemoteDecisionParseError::BadField("updated_permissions")),
    }
    match object.get("message") {
        None | Some(Value::Null) => {}
        Some(Value::String(message)) => decision.message = Some(message.clone()),
        Some(_) => return Err(RemoteDecisionParseError::BadField("message")),
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use RebonPermissionOption::{AllowAlways, AllowOnce, RejectOnce};
    use RemotePermissionRefusal::{
        PersistentDenyUnsupported, PersistentRuleNotAllowed, UpdatedInputNotAllowed,
        UpdatedInputOnDeny,
    };

    #[test]
    fn option_ids_match_rebon() {
        let ids: Vec<&str> = RebonPermissionOption::ALL
            .iter()
            .map(|option| option.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "allow_once",
                "allow_always",
                "allow_always_generalized",
                "reject_once"
            ]
        );
        for option in RebonPermissionOption::ALL {
            assert_eq!(RebonPermissionOption::parse(option.as_str()), Some(option));
            assert_eq!(option.to_string(), option.as_str());
        }
        assert_eq!(RebonPermissionOption::parse("reject_always"), None);
        assert_eq!(
            RebonPermissionOption::ALL.map(RebonPermissionOption::allows),
            [true, true, true, false]
        );
        assert_eq!(
            RebonPermissionOption::ALL.map(RebonPermissionOption::persists),
            [false, true, true, false]
        );
    }

    #[test]
    fn the_default_policy_is_one_shot() {
        assert_eq!(
            RemotePermissionPolicy::default(),
            RemotePermissionPolicy::one_shot()
        );
        let everything = RemotePermissionPolicy::one_shot()
            .allowing_persistent_rules()
            .allowing_updated_input();
        assert!(everything.allow_persistent_rules && everything.allow_updated_input);
    }

    /// Every combination of behavior × rules × edited input × the two
    /// policy switches: 32 rows, written out rather than computed, so the
    /// table in the module docs is what is actually tested.
    #[test]
    fn the_mapping_table_is_exhaustive() {
        // (allow, rules, input, persist_ok, input_ok) → expected
        type Row = (
            bool,
            bool,
            bool,
            bool,
            bool,
            Result<RebonPermissionOption, RemotePermissionRefusal>,
        );
        let rows: [Row; 32] = [
            // allow, no rules, no input: always once.
            (true, false, false, false, false, Ok(AllowOnce)),
            (true, false, false, false, true, Ok(AllowOnce)),
            (true, false, false, true, false, Ok(AllowOnce)),
            (true, false, false, true, true, Ok(AllowOnce)),
            // allow, no rules, edited input: needs the input switch.
            (true, false, true, false, false, Err(UpdatedInputNotAllowed)),
            (true, false, true, false, true, Ok(AllowOnce)),
            (true, false, true, true, false, Err(UpdatedInputNotAllowed)),
            (true, false, true, true, true, Ok(AllowOnce)),
            // allow, rules, no input: needs the rules switch.
            (
                true,
                true,
                false,
                false,
                false,
                Err(PersistentRuleNotAllowed),
            ),
            (
                true,
                true,
                false,
                false,
                true,
                Err(PersistentRuleNotAllowed),
            ),
            (true, true, false, true, false, Ok(AllowAlways)),
            (true, true, false, true, true, Ok(AllowAlways)),
            // allow, rules, edited input: rules are judged first.
            (
                true,
                true,
                true,
                false,
                false,
                Err(PersistentRuleNotAllowed),
            ),
            (true, true, true, false, true, Err(PersistentRuleNotAllowed)),
            (true, true, true, true, false, Err(UpdatedInputNotAllowed)),
            (true, true, true, true, true, Ok(AllowAlways)),
            // deny, no rules, no input: always once.
            (false, false, false, false, false, Ok(RejectOnce)),
            (false, false, false, false, true, Ok(RejectOnce)),
            (false, false, false, true, false, Ok(RejectOnce)),
            (false, false, false, true, true, Ok(RejectOnce)),
            // deny with an edited input: meaningless, whatever the policy.
            (false, false, true, false, false, Err(UpdatedInputOnDeny)),
            (false, false, true, false, true, Err(UpdatedInputOnDeny)),
            (false, false, true, true, false, Err(UpdatedInputOnDeny)),
            (false, false, true, true, true, Err(UpdatedInputOnDeny)),
            // deny always: refused by one-shot, unrepresentable otherwise.
            (
                false,
                true,
                false,
                false,
                false,
                Err(PersistentRuleNotAllowed),
            ),
            (
                false,
                true,
                false,
                false,
                true,
                Err(PersistentRuleNotAllowed),
            ),
            (
                false,
                true,
                false,
                true,
                false,
                Err(PersistentDenyUnsupported),
            ),
            (
                false,
                true,
                false,
                true,
                true,
                Err(PersistentDenyUnsupported),
            ),
            (false, true, true, false, false, Err(UpdatedInputOnDeny)),
            (false, true, true, false, true, Err(UpdatedInputOnDeny)),
            (false, true, true, true, false, Err(UpdatedInputOnDeny)),
            (false, true, true, true, true, Err(UpdatedInputOnDeny)),
        ];

        let mut seen = std::collections::HashSet::new();
        for (allow, rules, input, persist_ok, input_ok, expected) in rows {
            assert!(
                seen.insert((allow, rules, input, persist_ok, input_ok)),
                "duplicate row"
            );
            let mut decision = BridgePermissionResponse::new(if allow {
                BridgePermissionBehavior::Allow
            } else {
                BridgePermissionBehavior::Deny
            })
            .with_message("from the phone");
            if rules {
                decision =
                    decision.with_updated_permissions(OpaqueJson::new(r#"[{"rule":"Bash"}]"#));
            }
            if input {
                decision = decision.with_updated_input(OpaqueJson::new(r#"{"command":"ls"}"#));
            }
            let policy = RemotePermissionPolicy {
                allow_persistent_rules: persist_ok,
                allow_updated_input: input_ok,
            };
            let got = map_remote_decision(&decision, policy);
            let row = (allow, rules, input, persist_ok, input_ok);
            match expected {
                Ok(option) => {
                    let answer = got.unwrap_or_else(|refusal| panic!("{row:?}: {refusal:?}"));
                    assert_eq!(answer.option, option, "{row:?}");
                    assert_eq!(answer.updated_input, decision.updated_input, "{row:?}");
                    assert_eq!(answer.message.as_deref(), Some("from the phone"));
                    // Nothing persists unless the policy allowed it.
                    assert!(!answer.option.persists() || persist_ok, "{row:?}");
                    assert_ne!(answer.option, RebonPermissionOption::AllowAlwaysGeneralized);
                }
                Err(refusal) => assert_eq!(got, Err(refusal), "{row:?}"),
            }
        }
        assert_eq!(seen.len(), 32);
    }

    #[test]
    fn every_refusal_explains_itself() {
        for refusal in [
            PersistentRuleNotAllowed,
            PersistentDenyUnsupported,
            UpdatedInputNotAllowed,
            UpdatedInputOnDeny,
        ] {
            assert!(!refusal.to_string().is_empty());
        }
    }

    #[test]
    fn a_decision_parses_from_either_spelling() {
        let snake = parse_remote_decision(&json!({
            "behavior": "allow",
            "updated_input": {"command": "ls"},
            "updated_permissions": [{"type": "addRules"}],
            "message": "ok"
        }))
        .unwrap();
        assert_eq!(snake.behavior, BridgePermissionBehavior::Allow);
        assert_eq!(
            snake.updated_input,
            Some(OpaqueJson::new(r#"{"command":"ls"}"#))
        );
        assert_eq!(
            snake.updated_permissions,
            Some(OpaqueJson::new(r#"[{"type":"addRules"}]"#))
        );
        assert_eq!(snake.message.as_deref(), Some("ok"));

        let camel = parse_remote_decision(&json!({
            "behavior": "allow",
            "updatedInput": {"command": "ls"},
            "updatedPermissions": [{"type": "addRules"}],
            "message": "ok"
        }))
        .unwrap();
        assert_eq!(camel, snake);

        // The minimal decision, with nulls and an empty rule list.
        let plain = parse_remote_decision(&json!({
            "behavior": "deny",
            "updated_input": null,
            "updatedPermissions": [],
            "message": null,
            "interrupt": true
        }))
        .unwrap();
        assert_eq!(
            plain,
            BridgePermissionResponse::new(BridgePermissionBehavior::Deny)
        );
        assert_eq!(
            map_remote_decision(&plain, RemotePermissionPolicy::one_shot())
                .unwrap()
                .option,
            RejectOnce
        );
    }

    #[test]
    fn a_malformed_decision_is_not_guessed_at() {
        let cases = [
            (json!("allow"), RemoteDecisionParseError::NotAnObject),
            (json!({}), RemoteDecisionParseError::MissingBehavior),
            (
                json!({"behavior": "ask"}),
                RemoteDecisionParseError::UnknownBehavior("ask".into()),
            ),
            (
                json!({"behavior": true}),
                RemoteDecisionParseError::BadField("behavior"),
            ),
            (
                json!({"behavior": "allow", "updated_permissions": {"rule": 1}}),
                RemoteDecisionParseError::BadField("updated_permissions"),
            ),
            (
                json!({"behavior": "deny", "message": 7}),
                RemoteDecisionParseError::BadField("message"),
            ),
            (
                json!({"behavior": "allow", "updated_input": {}, "updatedInput": {}}),
                RemoteDecisionParseError::ConflictingSpellings("updated_input"),
            ),
            (
                json!({"behavior": "allow", "updated_permissions": [1], "updatedPermissions": [1]}),
                RemoteDecisionParseError::ConflictingSpellings("updated_permissions"),
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                parse_remote_decision(&value),
                Err(expected.clone()),
                "{value}"
            );
            assert!(!expected.to_string().is_empty());
        }
    }

    #[test]
    fn a_remote_always_is_refused_end_to_end_by_default() {
        // What a phone sends for "always allow", read and mapped the way a
        // runner would.
        let decision = parse_remote_decision(&json!({
            "behavior": "allow",
            "updatedPermissions": [{"type": "addRules", "rules": [{"toolName": "Bash"}]}]
        }))
        .unwrap();
        assert_eq!(
            map_remote_decision(&decision, RemotePermissionPolicy::default()),
            Err(PersistentRuleNotAllowed)
        );
        assert_eq!(
            map_remote_decision(
                &decision,
                RemotePermissionPolicy::one_shot().allowing_persistent_rules()
            )
            .unwrap()
            .option,
            AllowAlways
        );
    }
}
