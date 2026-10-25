//! Upstream DeepSeek Harness packages, unchanged from npm, on rebon's plane.
//!
//! The sample bundle (`tests/fixtures/dsh-upstream`) names thirteen current
//! dsh packages in dsh's own bundle format. Installing it is what
//! `rebon plugin install` does with such a package: the tree comes from the
//! npm registry through `verve::npm`, the probe mounts every row in one
//! realm and writes the manifest. Then the rows load on a real plane, in a
//! configured order that is the reverse of what they need, and each is used
//! the way rebon uses it:
//!
//! * `dsh-tool-fs` over `dsh-fs-local` — `read` and `write` called as rebon
//!   calls a composition tool, against the session's workspace;
//! * `dsh-settings` over rebon's configuration editor — `settings.describe`
//!   and `settings.mutate` through the client contract, persisted to
//!   `config.json` and applied live;
//! * `dsh-web-search-deepseek` — a search through its plane service, sent to
//!   the endpoint the settings write configured;
//! * `dsh-storage` + `dsh-storage-json`, `dsh-skill` +
//!   `dsh-skill-filesystem` — reached by a fixture consumer in the same realm;
//! * `dsh-tool-skill` — named as not running: it needs dsh's agent loop;
//! * `dsh-pwsh-local` + `dsh-tool-pwsh` — named as not running: they need a
//!   `subprocess` provider, and dsh's own (`dsh-subprocess-local`) is native
//!   addons and `child_process`, which a plugin on Verve does not get.
//!
//! It fetches from registry.npmjs.org and needs the runner (`REBON_VERVE`,
//! or `cargo build -p rebon-verve-host --bin rebon-verve`), so it is run by
//! hand: `cargo test -p rebon-session-runtime --test dsh_upstream --
//! --ignored`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_harness::rebon_kernel::Kernel;
use rebon_harness::rebon_plugin_package::{KernelPluginDeclaration, PluginManifest};
use rebon_kernel_seats::kernel_compose_tools::{set_process_compose_tools, ComposeToolRegistry};
use rebon_kernel_seats::kernel_config_seats::ConfigSeatsPlugin;
use rebon_kernel_seats::kernel_prompt_sections::{ComposePromptSections, SYSTEM_PROMPT_SERVICE};
use rebon_plugin_host::plugin_composition::{compose_section, CompositionRoots};
use rebon_plugin_host::plugin_manifests::PayloadManifests;
use rebon_plugin_host::plugin_plane::{PluginPlane, PluginPlaneConfig, REMOTE_EVENT_TOPIC};
use rebon_plugin_supervisor::{ToolInvocation, ToolInvoker, ToolRefusal};
use rebon_session_runtime::plugin::dsh_npm::adapt_with;
use serde_json::{json, Value};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves")
}

fn plain(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
}

fn runner() -> PathBuf {
    if let Some(path) = std::env::var_os("REBON_VERVE") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().expect("the test knows its own path");
    let name = if cfg!(windows) {
        "rebon-verve.exe"
    } else {
        "rebon-verve"
    };
    let built = exe
        .parent()
        .and_then(Path::parent)
        .expect("a test binary lives in target/<profile>/deps")
        .join(name);
    assert!(
        built.is_file(),
        "the probe needs the runner: build it with `cargo build -p rebon-verve-host --bin \
         rebon-verve`, or set REBON_VERVE"
    );
    built
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// No rebon tool is invoked by these packages; one that tried is recorded.
struct NoTools(Mutex<Vec<String>>);

impl ToolInvoker for NoTools {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<rebon_plugin_protocol::Payload, ToolRefusal>>
                + Send
                + '_,
        >,
    > {
        self.0.lock().unwrap().push(invocation.tool.clone());
        Box::pin(async { Err(ToolRefusal::new("[UNAVAILABLE_TOOL]", "not in this test")) })
    }
}

/// One canned Anthropic Messages answer with a native web-search result, and
/// every request it was sent, so a test can see which key and body arrived.
async fn fake_messages_api() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    let Ok(n) = socket.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let Some(head) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let length = text[..head]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= head + 4 + length {
                        log.lock().unwrap().push(text);
                        break;
                    }
                }
                let body = json!({
                    "content": [
                        { "type": "web_search_tool_result", "content": [
                            { "type": "web_search_result", "url": "https://rebon.example/plane", "title": "The plane" },
                        ] },
                        { "type": "text", "text": "x", "citations": [
                            { "url": "https://rebon.example/plane", "cited_text": "rebon hosts dsh" },
                        ] },
                    ]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, seen)
}

fn remote_value(answer: &Value) -> &Value {
    assert_eq!(answer["ok"], true, "{answer}");
    &answer["value"]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "fetches from registry.npmjs.org"]
async fn upstream_dsh_packages_load_unchanged_and_work_on_the_plane() {
    let runner = runner();
    let repo = repo();
    let compose_root = repo.join("runtimes/node/compose-runtime");
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("home");
    let workspace = scratch.path().join("workspace");
    let storage = scratch.path().join("storage");
    std::fs::create_dir_all(workspace.join(".agents/skills/hello")).unwrap();
    std::fs::write(
        workspace.join(".agents/skills/hello/SKILL.md"),
        "---\nname: hello\ndescription: Say hello\n---\nSay hello politely.\n",
    )
    .unwrap();
    std::fs::write(workspace.join("notes.txt"), "line one\nline two\n").unwrap();

    // ---- install: the bundle, as `rebon plugin install` adapts one ----
    let bundle = scratch.path().join("bundle");
    copy_dir(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsh-upstream"),
        &bundle,
    );
    let overrides = json!({
        "storage-json": { "root": plain(&storage) },
        "skill-filesystem": { "dshHome": plain(&home.join("dsh")), "agentsHome": plain(&home.join("agents")) },
    });
    let adapted = adapt_with(&bundle, Some(&overrides), &runner, &compose_root)
        .unwrap_or_else(|error| panic!("the bundle adapts: {error:#}"));
    // `REBON_DSH_BUNDLE_OUT` keeps the installed bundle, for checking another
    // embedder's plane against the same packages (the desktop app's).
    if let Some(out) = std::env::var_os("REBON_DSH_BUNDLE_OUT") {
        copy_dir(&bundle, Path::new(&out));
    }
    let mut rows = adapted.rows.clone();
    rows.sort();
    assert_eq!(
        rows,
        [
            "fs-local",
            "settings",
            "shell-env",
            "skill",
            "skill-filesystem",
            "storage",
            "storage-json",
            "tool-fs",
            "web-fetch-http",
            "web-search-deepseek",
        ],
        "skipped: {:?}",
        adapted.skipped
    );
    let skipped: BTreeMap<&str, &str> = adapted
        .skipped
        .iter()
        .map(|row| (row.id.as_str(), row.code.as_str()))
        .collect();
    assert_eq!(
        skipped,
        BTreeMap::from([
            ("pwsh-local", "[MISSING_INJECT]"),
            ("tool-pwsh", "[MISSING_INJECT]"),
            ("tool-skill", "[MISSING_INJECT]"),
        ])
    );
    let reason = |id: &str| {
        adapted
            .skipped
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.reason.clone())
            .unwrap_or_default()
    };
    assert!(
        reason("pwsh-local").contains("\"subprocess\""),
        "{}",
        reason("pwsh-local")
    );
    assert!(
        reason("tool-skill").contains("\"agents\""),
        "{}",
        reason("tool-skill")
    );
    for tool in ["read", "write", "edit"] {
        assert!(
            adapted.tools.iter().any(|t| t == tool),
            "{tool}: {:?}",
            adapted.tools
        );
    }
    let manifest = PluginManifest::load_from_dir(&bundle).expect("the bundle's manifest reads");
    assert_eq!(
        manifest.capabilities.kernel_plugins["fs-local"].provides,
        vec!["fs"]
    );
    assert!(manifest.capabilities.kernel_plugins["tool-fs"]
        .inject
        .contains(&"fs".to_owned()));

    // ---- compose: the rows in reverse of what they need, trusted ----
    let config_dir = scratch.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let configs = manifest
        .metadata
        .get("kernelPluginConfig")
        .cloned()
        .unwrap_or(Value::Null);
    let mut ids: Vec<String> = manifest
        .capabilities
        .kernel_plugins
        .keys()
        .cloned()
        .collect();
    ids.reverse();
    ids.push("dsh-seam-probe".into());
    let plugins: Vec<Value> = ids
        .iter()
        .map(|id| match configs.get(id) {
            Some(config) => json!({ "id": id, "config": config }),
            None => json!({ "id": id }),
        })
        .collect();
    let probe_module =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsh-seam-probe/index.mjs");
    let section = json!({
        "plugins": plugins,
        "modules": { "dsh-seam-probe": plain(&probe_module) },
        "trusted": ids,
    });
    std::fs::write(
        config_dir.join("config.json"),
        serde_json::to_vec_pretty(&json!({ "kernelPlugins": section })).unwrap(),
    )
    .unwrap();
    let installed: Vec<KernelPluginDeclaration> = manifest
        .capabilities
        .kernel_plugins
        .iter()
        .map(|(name, kernel)| KernelPluginDeclaration {
            name: name.clone(),
            root: bundle.clone(),
            manifest: kernel.clone(),
            source: None,
        })
        .collect();
    let roots = CompositionRoots {
        payload: compose_root.join("payload"),
        runtime: compose_root.clone(),
    };
    let manifests = PayloadManifests::load(&compose_root).unwrap();
    let composition = compose_section(&section, &roots, &[], &manifests, &installed, &config_dir);
    assert!(composition.skipped.is_empty(), "{:?}", composition.skipped);
    let order: Vec<&str> = composition
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    let at = |id: &str| order.iter().position(|x| *x == id).unwrap();
    assert!(at("fs-local") < at("tool-fs"), "{order:?}");
    assert!(at("storage") < at("storage-json"), "{order:?}");
    assert!(at("skill") < at("skill-filesystem"), "{order:?}");
    assert!(
        at("storage-json") < at("dsh-seam-probe") && at("skill") < at("dsh-seam-probe"),
        "{order:?}"
    );
    assert!(
        composition
            .entries
            .iter()
            .all(|entry| entry.container.is_none()),
        "trusted rows share the plane's realm"
    );

    // ---- boot a plane with them ----
    let kernel = Kernel::new();
    kernel
        .load(vec![Box::new(
            ConfigSeatsPlugin::new(config_dir.clone()).with_cwd(config_dir.clone()),
        )])
        .expect("kernel seats load");
    let ctx = kernel.context().fork("plane");
    let registry = ComposeToolRegistry::new(Vec::<String>::new());
    ctx.provide_json("tool-registry", registry.clone()).unwrap();
    set_process_compose_tools(registry.clone());
    ctx.provide_json(SYSTEM_PROMPT_SERVICE, ComposePromptSections::new())
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let heard = Arc::clone(&events);
    kernel.context().on_json(REMOTE_EVENT_TOPIC, move |event| {
        heard.lock().unwrap().push(event.clone());
    });
    let plane = PluginPlane::start(
        PluginPlaneConfig {
            runner,
            host_script: PathBuf::from(plain(&repo.join("runtimes/node/plugin-host/src/cli.mjs"))),
            loader: PathBuf::from(plain(&compose_root.join("src/index.mjs"))),
            compose_root: PathBuf::from(plain(&compose_root)),
            payload_dir: Some(PathBuf::from(plain(&compose_root.join("payload")))),
            structure: composition.structure.clone(),
            web: Value::Null,
            modules: composition.modules.clone(),
            // What a provider of the web seat may cause, as the real plane
            // exposes it: rebon's own search and fetch.
            exposed_tools: vec!["WebSearch".into(), "WebFetch".into()],
            exposed_seats: vec!["credentials".into(), "logger".into(), "settings".into()],
            tool_catalog: json!([]),
            scope_id: None,
            working_directory: workspace.clone(),
            unary_call_timeout: None,
            drain_deadline: None,
            lifecycle_sink: None,
        },
        ctx,
        registry,
        Arc::new(NoTools(Mutex::new(Vec::new()))) as Arc<dyn ToolInvoker>,
    )
    .await
    .expect("the plugin plane starts");
    for entry in &composition.entries {
        plane
            .load_entry(entry)
            .await
            .unwrap_or_else(|error| panic!("{} loads: {error}", entry.id));
    }

    // ---- dsh-tool-fs over dsh-fs-local ----
    let tools = plane.tool_dispatch();
    let call = |tool: &'static str, input: Value| {
        let tools = Arc::clone(&tools);
        async move { tools.serve(tool, input, Duration::from_secs(60)).await }
    };
    let read = call(
        "read",
        json!({ "file_path": plain(&workspace.join("notes.txt")) }),
    )
    .await
    .expect("read answers");
    assert!(read.to_string().contains("1: line one"), "{read}");
    let written = call(
        "write",
        json!({ "file_path": plain(&workspace.join("new.txt")), "content": "from dsh-tool-fs\n" }),
    )
    .await
    .expect("write answers");
    assert!(written.to_string().contains("Created file"), "{written}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("new.txt")).unwrap(),
        "from dsh-tool-fs\n"
    );
    let edited = call(
        "edit",
        json!({ "file_path": plain(&workspace.join("new.txt")), "old_string": "from", "new_string": "edited by" }),
    )
    .await;
    if cfg!(windows) {
        // On Windows dsh-fs-local replaces an existing file with ReplaceFileW
        // through koffi, a native addon, which Verve does not load.
        assert!(edited.is_err(), "{edited:?}");
    } else {
        edited.expect("edit answers");
        assert_eq!(
            std::fs::read_to_string(workspace.join("new.txt")).unwrap(),
            "edited by dsh-tool-fs\n"
        );
    }

    // ---- storage and skills, from a consumer in the same realm ----
    let stored = call("seam_probe", json!({ "action": "storage" }))
        .await
        .expect("the storage seam answers");
    assert!(stored.to_string().contains("\\\"n\\\":42"), "{stored}");
    assert!(
        storage.join("probe.json").is_file(),
        "dsh-storage-json wrote its unit"
    );
    let skills = call(
        "seam_probe",
        json!({ "action": "skills", "cwd": plain(&workspace) }),
    )
    .await
    .expect("the skills seam answers");
    assert!(skills.to_string().contains("hello"), "{skills}");

    // ---- settings through the client contract ----
    let described = plane.call_remote("settings/describe", json!({})).await;
    let namespaces = remote_value(&described)["namespaces"]
        .as_array()
        .unwrap()
        .clone();
    let search = namespaces
        .iter()
        .find(|view| view["ns"] == "web-search-deepseek")
        .unwrap_or_else(|| panic!("web-search-deepseek is a settings namespace: {described}"));
    let revision = search["revision"].as_u64().unwrap();
    assert!(search["schema"].to_string().contains("baseURL"), "{search}");
    let (api, requests) = fake_messages_api().await;
    let base_url = format!("http://{api}/v1");
    let mutated = plane
        .call_remote(
            "settings/mutate",
            json!({
                "ns": "web-search-deepseek",
                "ops": [
                    { "op": "set", "path": ["baseURL"], "value": base_url },
                    { "op": "set", "path": ["apiKey"], "value": "test-key" },
                    { "op": "set", "path": ["maxUses"], "value": 2 },
                ],
                "expectedRevision": revision,
            }),
        )
        .await;
    let view = remote_value(&mutated);
    assert_eq!(view["value"]["baseURL"], base_url.as_str());
    assert_eq!(view["value"]["maxUses"], 2);
    assert!(
        !mutated.to_string().contains("test-key"),
        "a secret never rides an answer: {mutated}"
    );
    let stale = plane
        .call_remote(
            "settings/mutate",
            json!({ "ns": "web-search-deepseek", "ops": [], "expectedRevision": revision }),
        )
        .await;
    assert_eq!(stale["error"]["code"], "settings/conflict", "{stale}");
    let config: Value =
        serde_json::from_slice(&std::fs::read(config_dir.join("config.json")).unwrap()).unwrap();
    let row = config["kernelPlugins"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "web-search-deepseek")
        .cloned()
        .unwrap();
    assert_eq!(row["config"]["baseURL"], base_url.as_str(), "{row}");
    assert_eq!(row["config"]["maxUses"], 2, "{row}");

    // ---- and the search goes where the settings said, live ----
    let result = plane
        .call_service(
            "web-search-deepseek",
            plane.scope(),
            "web:search:deepseek-official",
            json!({ "query": "rebon" }),
        )
        .await
        .expect("the search answers");
    assert_eq!(
        result["sources"][0]["url"], "https://rebon.example/plane",
        "{result}"
    );
    assert_eq!(result["sources"][0]["snippet"], "rebon hosts dsh");
    let sent = requests.lock().unwrap().join("\n");
    assert!(sent.contains("x-api-key: test-key"), "{sent}");
    assert!(sent.contains("\"max_uses\":2"), "{sent}");

    // ---- the rest of the contract ----
    let listed = plane
        .call_remote("pluginManager/listPlugins", json!({}))
        .await;
    assert!(
        remote_value(&listed)
            .as_array()
            .unwrap()
            .iter()
            .any(|plugin| plugin["entryId"] == "tool-fs"
                && plugin["moduleName"] == "@deepseek-ai/dsh-tool-fs"
                && plugin["fiberPhase"] == "active"),
        "{listed}"
    );
    let inventory = plane.call_remote("pluginInventory/list", json!({})).await;
    assert_eq!(
        remote_value(&inventory)["entries"]
            .as_array()
            .unwrap()
            .len(),
        composition.entries.len(),
        "{inventory}"
    );
    let described = plane
        .call_remote(
            "credentials/describe",
            json!({ "refs": ["DEEPSEEK_API_KEY"] }),
        )
        .await;
    assert_eq!(
        remote_value(&described)["DEEPSEEK_API_KEY"]["writable"],
        false
    );
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["event"]["event"] == "settings/document-updated"),
        "the settings write was announced to clients: {:?}",
        events.lock().unwrap()
    );

    plane.shutdown().await;
}
