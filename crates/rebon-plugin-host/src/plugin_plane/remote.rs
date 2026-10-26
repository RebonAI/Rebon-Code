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
//! client 通过 [`PluginPlane::open_remote`] 打开 `$events`。事件仍从现有
//! `KernelEvents` 入口只发布到内核 topic，再由每个 plane 的单一 scoped listener
//! 校验来源并写入有界 transport 队列。原始 topic 是
//! [`REMOTE_EVENT_TOPIC`](super::REMOTE_EVENT_TOPIC)。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rebon_kernel::{Context, Disposer};
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::PluginHostSupervisor;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::{ComposeEntry, PluginLifecycle, PluginPlane, COMPOSE_PLUGIN_ID, REMOTE_EVENT_TOPIC};

/// 慢客户端最多保留这些事件；丢失后必须重连并重读快照，不能继续使用过期 mirror。
const EVENT_BACKLOG: usize = 256;

/// DSH `$events` 的 Host 流。首项是 `ready`，后续为 `emit`，不需要 DOM。
///
/// `recv` 的错误也是 DSH wire envelope；丢失事件时只返回一次错误并结束。
/// 丢弃或调用 `close` 只释放 transport receiver；内核 listener 属于 plane，而非 client。
#[must_use]
pub struct RemoteEventStream {
    ready: Option<Value>,
    receiver: Option<broadcast::Receiver<Value>>,
}

impl RemoteEventStream {
    pub async fn recv(&mut self) -> Option<Result<Value, Value>> {
        if let Some(ready) = self.ready.take() {
            return Some(Ok(ready));
        }
        match self.receiver.as_mut()?.recv().await {
            Ok(frame) => Some(Ok(frame)),
            Err(broadcast::error::RecvError::Closed) => {
                self.close();
                None
            }
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                self.close();
                Some(Err(refused(
                    "gateway/internal",
                    format!(
                        "$events lost {dropped} events; reconnect and reload the remote snapshots"
                    ),
                )))
            }
        }
    }

    pub fn close(&mut self) {
        self.ready = None;
        self.receiver = None;
    }
}

/// kernel 到 client 的 transport 适配：每个 plane 一个 listener、一条有界队列。
/// `planeEpoch` 由认证 publisher 填入，不能用可重复的 scope 名或 payload 自报来源。
pub(super) struct RemoteEvents {
    listener: Context,
    sender: Arc<Mutex<Option<broadcast::Sender<Value>>>>,
    next: AtomicU64,
    plane_epoch: u64,
    main_epoch: u64,
    home: String,
}

impl RemoteEvents {
    pub(super) fn new(ctx: &Context, plane_epoch: u64, main_epoch: u64, home: String) -> Self {
        let listener = ctx.fork(&format!("remote-events-{plane_epoch}"));
        let (sender, _) = broadcast::channel(EVENT_BACKLOG);
        let sender = Arc::new(Mutex::new(Some(sender)));
        let transport = Arc::clone(&sender);
        listener.on_json(REMOTE_EVENT_TOPIC, move |published| {
            if published["planeEpoch"].as_u64() != Some(plane_epoch)
                || published["pluginId"].as_str() != Some(COMPOSE_PLUGIN_ID)
            {
                return;
            }
            let event = &published["event"];
            let (Some(name), Some(args)) = (event["event"].as_str(), event["args"].as_array())
            else {
                return;
            };
            if let Some(sender) = transport
                .lock()
                .expect("remote event sender poisoned")
                .as_ref()
            {
                // 这只是 wire 队列，不是第二个域事件出口；没有 client 时可正常无人接收。
                let _ = sender.send(json!({ "type": "emit", "event": name, "args": args }));
            }
        });
        let transport = Arc::clone(&sender);
        listener.effect_labeled("close remote event transport", || {
            Disposer::new(move || {
                transport
                    .lock()
                    .expect("remote event sender poisoned")
                    .take();
            })
        });
        Self {
            listener,
            sender,
            next: AtomicU64::new(0),
            plane_epoch,
            main_epoch,
            home,
        }
    }

    fn open(&self, endpoint: &str, args: &Value) -> Result<RemoteEventStream, Value> {
        if endpoint != "$events" {
            return Err(refused(
                "gateway/method-unavailable",
                format!("no stream answers {endpoint}"),
            ));
        }
        if !args.as_object().is_some_and(|args| args.is_empty()) {
            return Err(refused(
                "gateway/bad-request",
                "$events expects empty named arguments",
            ));
        }
        let sender = self.sender.lock().expect("remote event sender poisoned");
        let Some(sender) = sender.as_ref() else {
            return Err(refused(
                "gateway/internal",
                "the plugin plane has shut down",
            ));
        };
        let client = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(RemoteEventStream {
            // 先订阅再交出 ready，启动期间的事件不会落在两个步骤之间。
            receiver: Some(sender.subscribe()),
            ready: Some(json!({
                "type": "ready",
                "clientId": format!("rebon:{}:{}:{client}", self.main_epoch, self.plane_epoch),
                "host": { "home": self.home },
            })),
        })
    }

    pub(super) fn close(&self) {
        // 注销唯一 listener 并释放 sender；已排队的帧读尽后 EOF。父域 dispose 同样生效。
        self.listener.dispose();
    }
}

impl Drop for RemoteEvents {
    fn drop(&mut self) {
        // 启动失败或遗漏显式 shutdown 也不能把 listener 留在父 Context 上。
        self.close();
    }
}

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
        if !args.is_object() {
            return refused("gateway/bad-request", "remote calls expect named arguments");
        }
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

    /// 打开 client carrier 的 `$events`，参数与 `call_remote` 一样为命名参数。
    /// 不提供的 stream（包括权限 waterfall）会明确拒绝，不伪装成空流。
    pub fn open_remote(&self, endpoint: &str, args: Value) -> Result<RemoteEventStream, Value> {
        self.template.events.remote_events.open(endpoint, &args)
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
    use rebon_kernel::Kernel;
    use rebon_plugin_protocol::CallIdentity;
    use rebon_plugin_supervisor::{EventPublisher, PublishedEvent};

    fn publication(host_epoch: u64, plugin_id: &str, body: Value) -> PublishedEvent {
        PublishedEvent {
            identity: CallIdentity {
                host_epoch,
                plugin_id: plugin_id.into(),
                scope_id: "rebon:plane".into(),
                scope_generation: 1,
                call_id: "event:1".into(),
            },
            topic: REMOTE_EVENT_TOPIC.into(),
            event: Payload::from(body),
        }
    }

    fn assert_empty(stream: &mut RemoteEventStream) {
        assert!(matches!(
            stream.receiver.as_mut().unwrap().try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_start_ready_then_emit_to_each_client_without_crossing_planes() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        // 相同 kernel、相同 Context 标签、相同 wire scope，甚至相同来源 host epoch。
        // 隔离依靠 publisher 的 plane provenance，不是 scope 名或 payload 自报值。
        let publisher =
            super::super::KernelEvents::new(ctx.fork("plane"), 7, "C:/rebon/dsh".into());
        let other = super::super::KernelEvents::new(ctx.fork("plane"), 7, "C:/other/dsh".into());
        assert_ne!(publisher.plane_epoch, other.plane_epoch);
        let events = &publisher.remote_events;
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 2);
        let mut first = events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        let mut second = events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        let mut unrelated = other
            .remote_events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 2);
        for revision in 2..5 {
            publisher
                .publish(publication(
                    7001,
                    COMPOSE_PLUGIN_ID,
                    json!({ "event": "settings/document-updated", "args": ["search", revision] }),
                ))
                .await
                .unwrap();
        }
        other
            .publish(publication(
                7001,
                COMPOSE_PLUGIN_ID,
                json!({ "planeEpoch": publisher.plane_epoch, "event": "settings/document-updated", "args": [99] }),
            ))
            .await
            .unwrap();
        let ready = first.recv().await.unwrap().unwrap();
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["host"]["home"], "C:/rebon/dsh");
        let second_ready = second.recv().await.unwrap().unwrap();
        assert_ne!(ready["clientId"], second_ready["clientId"]);
        for revision in 2..5 {
            let frame = json!({
                "type": "emit", "event": "settings/document-updated", "args": ["search", revision]
            });
            assert_eq!(first.recv().await.unwrap().unwrap(), frame);
            assert_eq!(second.recv().await.unwrap().unwrap(), frame);
        }
        let other_ready = unrelated.recv().await.unwrap().unwrap();
        assert_ne!(ready["clientId"], other_ready["clientId"]);
        assert_eq!(
            unrelated.recv().await.unwrap().unwrap()["args"],
            json!([99])
        );
        assert_empty(&mut first);
        assert_empty(&mut second);
        assert_empty(&mut unrelated);
        first.close();
        assert!(first.recv().await.is_none());
        drop(second);
        assert_eq!(
            events
                .sender
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .receiver_count(),
            0
        );
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 2);
        // 所有 client 离开后仍是同一个 listener；重连不新建域广播。
        let mut reconnected = events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        publisher
            .publish(publication(
                7002,
                COMPOSE_PLUGIN_ID,
                json!({ "event": "settings/document-updated", "args": [5] }),
            ))
            .await
            .unwrap();
        events.close();
        events.close();
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 1);
        assert_eq!(reconnected.recv().await.unwrap().unwrap()["type"], "ready");
        assert_eq!(
            reconnected.recv().await.unwrap().unwrap()["args"],
            json!([5])
        );
        assert!(reconnected.recv().await.is_none());
        assert!(events.open("$events", &json!({})).is_err());
        // 关闭一个 plane 不影响另一个 plane 的 listener 或流。
        other
            .publish(publication(
                7002,
                COMPOSE_PLUGIN_ID,
                json!({ "event": "settings/document-updated", "args": [100] }),
            ))
            .await
            .unwrap();
        other.remote_events.close();
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 0);
        assert_eq!(
            unrelated.recv().await.unwrap().unwrap()["args"],
            json!([100])
        );
        assert!(unrelated.recv().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_refuse_bad_arguments_and_end_on_lag_instead_of_hiding_loss() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        let publisher = super::super::KernelEvents::new(ctx.fork("plane"), 7, "home".into());
        let events = &publisher.remote_events;
        for args in [Value::Null, json!([]), json!({ "agentId": "x" })] {
            let error = events.open("$events", &args).err().unwrap();
            assert_eq!(error["error"]["code"], "gateway/bad-request");
        }
        assert_eq!(
            events.open("approval/request", &json!({})).err().unwrap()["error"]["code"],
            "gateway/method-unavailable"
        );
        let mut stream = events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        for revision in 0..EVENT_BACKLOG * 2 {
            publisher
                .publish(publication(
                    7,
                    COMPOSE_PLUGIN_ID,
                    json!({ "event": "settings/document-updated", "args": [revision] }),
                ))
                .await
                .unwrap();
        }
        assert_eq!(stream.recv().await.unwrap().unwrap()["type"], "ready");
        let error = stream.recv().await.unwrap().unwrap_err();
        assert_eq!(error["error"]["code"], "gateway/internal");
        assert!(stream.recv().await.is_none());
        assert_eq!(
            events
                .sender
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .receiver_count(),
            0
        );
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 1);
        events.close();
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_authenticated_control_events_reach_remote_clients() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&seen);
        ctx.on_json(REMOTE_EVENT_TOPIC, move |wrapper| {
            observed.lock().unwrap().push(wrapper.clone());
        });
        let publisher = super::super::KernelEvents::new(ctx.fork("plane"), 7, "home".into());
        let mut stream = publisher
            .remote_events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        stream.recv().await.unwrap().unwrap();
        let forged = json!({
            "planeEpoch": publisher.plane_epoch, "hostEpoch": 7, "pluginId": COMPOSE_PLUGIN_ID, "scopeId": "rebon:plane",
            "event": "settings/document-updated", "args": [1]
        });
        publisher
            .publish(publication(7001, "untrusted", forged.clone()))
            .await
            .unwrap();
        assert_empty(&mut stream);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [json!({
                "planeEpoch": publisher.plane_epoch, "hostEpoch": 7001, "pluginId": "untrusted", "scopeId": "rebon:plane",
                "event": forged
            })]
        );
        for body in [
            json!({}),
            json!({ "event": 1, "args": [] }),
            json!({ "event": "x", "args": {} }),
        ] {
            publisher
                .publish(publication(7001, COMPOSE_PLUGIN_ID, body))
                .await
                .unwrap();
        }
        let mut wrong_topic = publication(7001, COMPOSE_PLUGIN_ID, forged);
        wrong_topic.topic = "other:event".into();
        publisher.publish(wrong_topic).await.unwrap();
        // 缺少 plane provenance 的旧 wrapper 也不能进入 transport。
        ctx.emit_json(
            REMOTE_EVENT_TOPIC,
            &json!({
                "pluginId": COMPOSE_PLUGIN_ID, "scopeId": "rebon:plane",
                "event": { "event": "settings/document-updated", "args": [1] }
            }),
        );
        assert_empty(&mut stream);
        let body = json!({ "event": "settings/document-updated", "args": [2] });
        publisher
            .publish(publication(7002, COMPOSE_PLUGIN_ID, body.clone()))
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap().last().unwrap(),
            &json!({
                "planeEpoch": publisher.plane_epoch, "hostEpoch": 7002, "pluginId": COMPOSE_PLUGIN_ID,
                "scopeId": "rebon:plane", "event": body
            })
        );
        assert_eq!(
            stream.recv().await.unwrap().unwrap(),
            json!({
                "type": "emit", "event": "settings/document-updated", "args": [2]
            })
        );
        assert_empty(&mut stream);
        publisher.remote_events.close();
        // 只移除 plane 的 listener，既有内核观察者不受影响。
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 1);
        assert!(stream.recv().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transport_is_fed_by_kernel_listener_not_publisher() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        let client = Arc::new(Mutex::new(None::<RemoteEventStream>));
        let probe = Arc::clone(&client);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&seen);
        // 在 transport listener 之前注册。若 publisher 旁仍有直接投递，队列已非空。
        ctx.on_json(REMOTE_EVENT_TOPIC, move |_| {
            let client = probe.lock().unwrap();
            let empty = client
                .as_ref()
                .unwrap()
                .receiver
                .as_ref()
                .unwrap()
                .is_empty();
            // kernel 会隔离回调 panic；把结果带出去断言，避免测试假绿。
            observed.lock().unwrap().push(empty);
        });
        let publisher = super::super::KernelEvents::new(ctx.fork("plane"), 7, "home".into());
        *client.lock().unwrap() = Some(
            publisher
                .remote_events
                .open("$events", &json!({}))
                .unwrap_or_else(|e| panic!("{e}")),
        );
        publisher
            .publish(publication(
                7,
                COMPOSE_PLUGIN_ID,
                json!({ "event": "settings/document-updated", "args": [1] }),
            ))
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), [true]);
        let mut stream = client.lock().unwrap().take().unwrap();
        assert_eq!(stream.recv().await.unwrap().unwrap()["type"], "ready");
        assert_eq!(stream.recv().await.unwrap().unwrap()["args"], json!([1]));
        assert_empty(&mut stream);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transport_drop_and_parent_disposal_unregister_listener_and_end_stream() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("plane");
        let publisher = super::super::KernelEvents::new(ctx.clone(), 7, "home".into());
        let mut stream = publisher
            .remote_events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        let sender = Arc::downgrade(&publisher.remote_events.sender);
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 1);
        drop(publisher);
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 0);
        assert!(sender.upgrade().is_none());
        assert_eq!(stream.recv().await.unwrap().unwrap()["type"], "ready");
        assert!(stream.recv().await.is_none());

        let publisher = super::super::KernelEvents::new(ctx.clone(), 8, "home".into());
        let mut stream = publisher
            .remote_events
            .open("$events", &json!({}))
            .unwrap_or_else(|e| panic!("{e}"));
        ctx.dispose();
        assert_eq!(ctx.json_listener_count(REMOTE_EVENT_TOPIC), 0);
        assert!(publisher.remote_events.open("$events", &json!({})).is_err());
        assert_eq!(stream.recv().await.unwrap().unwrap()["type"], "ready");
        assert!(stream.recv().await.is_none());
    }

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
