//! DeepSeek Harness's client contract, answered by the plane.
//!
//! A dsh client plugin — the browser half of a package, `lib/client.js` —
//! reaches its Host with `ctx.remote.<namespace>.<method>(…)`. In dsh that is
//! the gateway (`@deepseek-ai/dsh-api-gateway`): it maps the call onto a Host
//! service with exact named arguments and answers
//! `{ ok: true, value }` or `{ ok: false, error: { code, message, details } }`,
//! never a thrown carrier fault. In rebon the plane is the Host, so this is
//! the gateway: [`PluginPlane::call_remote`] takes the endpoint
//! (`settings/describe`) and the named arguments, and answers in that shape.
//!
//! Where an answer comes from follows where the fact lives:
//!
//! * **the realm** — settings forms, credential descriptions, the mounted
//!   entries, their client halves — is asked through the composition's own
//!   `compose` service (`compose-runtime/src/remote.mjs`). A realm is per
//!   host, so a namespace is asked of the host serving its entry, and a
//!   listing is gathered from every host;
//! * **the plane** — which entries are configured and how far each got
//!   (`pluginManager`), and whether a credential is set (`credentials`) —
//!   is answered here. Credentials especially: describing one never needs
//!   its value, so no realm is asked for it.
//!
//! The events a client hears are not a call: the composition publishes them
//! on [`REMOTE_EVENT_TOPIC`](super::REMOTE_EVENT_TOPIC), and whoever carries
//! the client subscribes to that topic on the kernel's event plane.

use std::path::Path;
use std::sync::Arc;

use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::PluginHostSupervisor;
use serde_json::{json, Value};

use super::{ComposeEntry, PluginLifecycle, PluginPlane, COMPOSE_PLUGIN_ID};

/// The endpoints whose answer is gathered from every realm rather than asked
/// of one.
const GATHERED: &[&str] = &[
    "settings/describe",
    "pluginInventory/list",
    "rebon/clientModules",
];

/// At most this many references in one `credentials.describe`, as dsh's
/// controller allows.
const MAX_DESCRIBE_REFS: usize = 64;

/// A remote failure in dsh's wire shape.
fn refused(code: &str, message: impl Into<String>) -> Value {
    json!({ "ok": false, "error": { "code": code, "message": message.into(), "details": {} } })
}

fn succeeded(value: Value) -> Value {
    json!({ "ok": true, "value": value })
}

fn is_ok(answer: &Value) -> bool {
    answer.get("ok").and_then(Value::as_bool) == Some(true)
}

/// dsh's fiber phase for a plane lifecycle state.
fn phase(state: &PluginLifecycle) -> Value {
    match state {
        PluginLifecycle::Loading => json!("loading"),
        PluginLifecycle::Ready => json!("active"),
        PluginLifecycle::Draining => json!("unloading"),
        PluginLifecycle::Unloaded => Value::Null,
        _ => json!("failed"),
    }
}

/// The name of the package an entry's module belongs to: the nearest
/// `package.json` above the module, inside the entry's root.
fn module_name(entry: &ComposeEntry) -> String {
    let root = Path::new(&entry.root);
    let mut dir = root.join(&entry.entry);
    while dir.pop() {
        if let Some(name) = std::fs::read(dir.join("package.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|package| package.get("name")?.as_str().map(str::to_owned))
        {
            return name;
        }
        if dir == root {
            break;
        }
    }
    entry.id.clone()
}

impl PluginPlane {
    /// Answers one DeepSeek Harness remote call, in dsh's wire shape.
    ///
    /// `args` are the named wire arguments (`{ "ns": …, "ops": … }`), as
    /// dsh's client sends them. Never an `Err`: a host that cannot be
    /// reached is a `gateway/internal` answer, as dsh's gateway reports a
    /// provider failure.
    pub async fn call_remote(&self, endpoint: &str, args: Value) -> Value {
        if let Some(method) = endpoint.strip_prefix("pluginManager/") {
            return self.plugin_manager(method);
        }
        if let Some(method) = endpoint.strip_prefix("credentials/") {
            return credentials(method, &args, &|name| std::env::var_os(name));
        }
        if GATHERED.contains(&endpoint) {
            return self.gather(endpoint, &args).await;
        }
        // A settings write belongs to the realm its entry is mounted in.
        let host = match args.get("ns").and_then(Value::as_str) {
            Some(ns) if endpoint.starts_with("settings/") => self.hosts.for_plugin(ns),
            _ => Arc::clone(self.hosts.main()),
        };
        self.ask(&host, endpoint, &args).await
    }

    /// Every host holding a realm: the shared one and each container's.
    async fn realm_hosts(&self) -> Vec<Arc<PluginHostSupervisor>> {
        let mut hosts = vec![Arc::clone(self.hosts.main())];
        hosts.extend(
            self.containers
                .lock()
                .await
                .values()
                .map(|container| Arc::clone(&container.supervisor)),
        );
        hosts
    }

    /// One realm's answer.
    async fn ask(&self, host: &PluginHostSupervisor, endpoint: &str, args: &Value) -> Value {
        let request = json!({ "kind": "remote", "endpoint": endpoint, "args": args });
        let answer = host
            .call_service_bounded(
                COMPOSE_PLUGIN_ID,
                &self.scope,
                "compose",
                Payload::from(request),
                Some(self.unary_call_timeout),
            )
            .await;
        match answer.map(|payload| payload.to_value()) {
            Ok(Ok(value)) if value.get("ok").is_some() => value,
            Ok(Ok(other)) => refused(
                "gateway/internal",
                format!("the composition answered {endpoint} with {other}"),
            ),
            Ok(Err(error)) => refused("gateway/internal", format!("{error}")),
            Err(error) => refused("gateway/internal", format!("{error}")),
        }
    }

    /// A listing endpoint, gathered from every realm: the namespaces or
    /// entries of each, in host order. Realms that do not hold the service
    /// add nothing; if none does, the first refusal is the answer.
    async fn gather(&self, endpoint: &str, args: &Value) -> Value {
        let mut answers = Vec::new();
        for host in self.realm_hosts().await {
            answers.push(self.ask(&host, endpoint, args).await);
        }
        let Some(first) = answers.iter().find(|answer| is_ok(answer)).cloned() else {
            return answers
                .into_iter()
                .next()
                .unwrap_or_else(|| refused("gateway/internal", "no realm is running"));
        };
        let list_key = match endpoint {
            "settings/describe" => "namespaces",
            _ => "entries",
        };
        let mut value = first["value"].clone();
        let mut merged: Vec<Value> = Vec::new();
        let mut writable = true;
        for answer in answers.iter().filter(|answer| is_ok(answer)) {
            if let Some(list) = answer["value"][list_key].as_array() {
                merged.extend(list.iter().cloned());
            }
            if answer["value"].get("writable") == Some(&Value::Bool(false)) {
                writable = false;
            }
        }
        value[list_key] = Value::Array(merged);
        if endpoint == "settings/describe" {
            value["writable"] = Value::Bool(writable);
        }
        succeeded(value)
    }

    /// The `pluginManager` namespace: what the plane has loaded, read-only.
    ///
    /// dsh's plugin manager edits the profile; rebon's composition is edited
    /// where rebon's plugins are configured, so every entry here is
    /// `readOnlyReason: "management-required"` and the mutating methods are
    /// unavailable rather than half-done.
    fn plugin_manager(&self, method: &str) -> Value {
        match method {
            "listPlugins" => {
                let lifecycles = self.lifecycles();
                let mut entries = self.loaded_entries();
                entries.sort_by(|a, b| a.id.cmp(&b.id));
                let plugins: Vec<Value> = entries
                    .iter()
                    .map(|entry| {
                        json!({
                            "entryId": entry.id,
                            "moduleName": module_name(entry),
                            "enabled": true,
                            "fiberPhase": lifecycles
                                .get(&entry.id)
                                .map(|record| phase(&record.state))
                                .unwrap_or(Value::Null),
                            "readOnlyReason": "management-required",
                        })
                    })
                    .collect();
                succeeded(Value::Array(plugins))
            }
            other => refused(
                "gateway/method-unavailable",
                format!(
                    "pluginManager.{other} changes the composition, which rebon configures in its own \
                     plugin settings; this plane only lists it"
                ),
            ),
        }
    }
}

/// A credential reference as dsh spells one: an environment variable name.
fn is_reference(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The `credentials` namespace: whether each reference is set, never what
/// it is set to. Writes are refused: rebon's credentials are set in rebon,
/// not from a page.
fn credentials(
    method: &str,
    args: &Value,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Value {
    match method {
        "describe" => {
            let refs: Option<Vec<&str>> = args
                .get("refs")
                .and_then(Value::as_array)
                .filter(|refs| refs.len() <= MAX_DESCRIBE_REFS)
                .and_then(|refs| refs.iter().map(Value::as_str).collect())
                .filter(|refs: &Vec<&str>| refs.iter().all(|name| is_reference(name)));
            let Some(refs) = refs else {
                return refused(
                    "gateway/bad-request",
                    format!(
                        "invalid payload for credentials.describe: `refs` is at most                          {MAX_DESCRIBE_REFS} environment variable names"
                    ),
                );
            };
            let views: serde_json::Map<String, Value> = refs
                .into_iter()
                .map(|name| {
                    let configured = env(name).is_some_and(|value| !value.is_empty());
                    let view = if configured {
                        json!({ "configured": true, "source": "env", "writable": false })
                    } else {
                        json!({ "configured": false, "writable": false })
                    };
                    (name.to_owned(), view)
                })
                .collect();
            succeeded(Value::Object(views))
        }
        "set" | "unset" => refused(
            "credential/rejected",
            "rebon keeps credentials itself: set them where rebon's providers are configured,              not from a page",
        ),
        other => refused(
            "gateway/method-unavailable",
            format!("credentials has no method {other:?}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a reference is set is all a page learns; the value never
    /// leaves the machine's environment, and a page cannot write one.
    #[test]
    fn credentials_are_described_and_never_shown_or_written() {
        let env = |name: &str| match name {
            "EXA_API_KEY" => Some(std::ffi::OsString::from("secret-value")),
            "EMPTY_KEY" => Some(std::ffi::OsString::new()),
            _ => None,
        };
        let answer = credentials(
            "describe",
            &json!({ "refs": ["EXA_API_KEY", "EMPTY_KEY", "MISSING_KEY"] }),
            &env,
        );
        assert_eq!(
            answer,
            json!({ "ok": true, "value": {
                "EXA_API_KEY": { "configured": true, "source": "env", "writable": false },
                "EMPTY_KEY": { "configured": false, "writable": false },
                "MISSING_KEY": { "configured": false, "writable": false },
            } })
        );
        assert!(!answer.to_string().contains("secret-value"));
        for bad in [
            json!({ "refs": ["not a ref"] }),
            json!({ "refs": "EXA_API_KEY" }),
            json!({ "refs": [1] }),
            json!({}),
            json!({ "refs": vec!["A"; MAX_DESCRIBE_REFS + 1] }),
        ] {
            assert_eq!(
                credentials("describe", &bad, &env)["error"]["code"],
                "gateway/bad-request",
                "{bad}"
            );
        }
        for write in ["set", "unset"] {
            assert_eq!(
                credentials(write, &json!({ "ref": "EXA_API_KEY", "value": "x" }), &env)["error"]
                    ["code"],
                "credential/rejected"
            );
        }
    }

    #[test]
    fn an_entry_is_named_by_the_package_its_module_is_in() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir
            .path()
            .join("node_modules/@deepseek-ai/dsh-tool-fs/lib/index.js");
        std::fs::create_dir_all(module.parent().unwrap()).unwrap();
        std::fs::write(&module, "export {}").unwrap();
        std::fs::write(
            dir.path()
                .join("node_modules/@deepseek-ai/dsh-tool-fs/package.json"),
            r#"{"name":"@deepseek-ai/dsh-tool-fs"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"bundle"}"#).unwrap();
        let entry = |module: &str| ComposeEntry {
            id: "tool-fs".into(),
            root: dir.path().to_string_lossy().into_owned(),
            entry: module.into(),
            ..ComposeEntry::default()
        };
        assert_eq!(
            module_name(&entry("node_modules/@deepseek-ai/dsh-tool-fs/lib/index.js")),
            "@deepseek-ai/dsh-tool-fs"
        );
        assert_eq!(module_name(&entry("index.js")), "bundle");
        let bare = ComposeEntry {
            id: "loose".into(),
            root: dir.path().join("nowhere").to_string_lossy().into_owned(),
            entry: "x.js".into(),
            ..ComposeEntry::default()
        };
        assert_eq!(module_name(&bare), "loose");
    }

    #[test]
    fn a_plane_lifecycle_reads_as_a_dsh_fiber_phase() {
        assert_eq!(phase(&PluginLifecycle::Ready), json!("active"));
        assert_eq!(phase(&PluginLifecycle::Loading), json!("loading"));
        assert_eq!(phase(&PluginLifecycle::Draining), json!("unloading"));
        assert_eq!(phase(&PluginLifecycle::Unloaded), Value::Null);
        assert_eq!(
            phase(&PluginLifecycle::Failed { reason: "x".into() }),
            json!("failed")
        );
    }
}
