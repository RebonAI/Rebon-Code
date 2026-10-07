//! A Claude Code mod in a container, through the `mods` seat.
//!
//! The seat's file, process and network calls run in rebon's own process, so
//! the container's Node permissions do not reach them; the seat keeps the
//! same contract for a contained mod itself. This drives a real mod on a real
//! plane and asks the seat for each kind of call.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt as _;

use rebon_kernel::Kernel;
use rebon_kernel_seats::kernel_compose_tools::ComposeToolRegistry;
use rebon_kernel_seats::kernel_config_seats::ConfigSeatsPlugin;
use rebon_kernel_seats::kernel_core_commands::CoreCommandsPlugin;
use rebon_kernel_seats::kernel_prompt_sections::{ComposePromptSections, SYSTEM_PROMPT_SERVICE};
use rebon_plugin_host::container::ContainerSpec;
use rebon_plugin_host::mods::compose::mod_entry;
use rebon_plugin_host::plugin_plane::{default_exposed_seats, PluginPlane, PluginPlaneConfig};
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::{ToolInvocation, ToolInvoker, ToolRefusal};
use serde_json::{json, Value};

fn node() -> Option<PathBuf> {
    match std::env::var_os("REBON_TEST_NODE") {
        Some(node) => Some(PathBuf::from(node)),
        None => {
            assert_ne!(
                std::env::var_os("REBON_REQUIRE_TEST_NODE").as_deref(),
                Some(std::ffi::OsStr::new("1")),
                "REBON_REQUIRE_TEST_NODE=1 requires an absolute REBON_TEST_NODE"
            );
            eprintln!("skipping: set REBON_TEST_NODE to an absolute Node executable");
            None
        }
    }
}

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

/// Stands in for rebon's tools: records what the mod was routed to, and
/// answers as a tool the person allowed would.
#[derive(Default)]
struct RecordingTools {
    seen: Mutex<Vec<(String, Value)>>,
}

impl ToolInvoker for RecordingTools {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        self.seen.lock().unwrap().push((
            invocation.tool.clone(),
            invocation.input.to_value().unwrap_or(Value::Null),
        ));
        Box::pin(async move { Ok(Payload::from(json!("ran through the tool"))) })
    }
}

const PROBE_MOD: &str = r#"
export const register = (on) => {
  on('session.start', async ($, e, next) => {
    await $.command.register({ name: 'probe', description: 'Reports what the seat allows' });
    return next(e);
  });
  on('command.run', { command: 'probe' }, async ($, e) => {
    const ask = JSON.parse(e.args);
    const attempt = async (run) => {
      try { return { ok: true, value: await run() }; }
      catch (error) { return { ok: false, code: String(error?.code ?? ''), message: String(error?.message ?? error) }; }
    };
    let answer;
    switch (ask.do) {
      case 'read': answer = await attempt(() => $.fs.read(ask.path)); break;
      case 'write': answer = await attempt(() => $.fs.write(ask.path, 'probe')); break;
      case 'run': answer = await attempt(() => $.process.run(ask.argv)); break;
      case 'fetch': answer = await attempt(() => $.http.fetch(ask.url)); break;
      default: answer = { ok: false, code: 'UNKNOWN' };
    }
    return { text: JSON.stringify(answer) };
  });
};
"#;

fn write_probe_mod(into: &Path) -> PathBuf {
    let root = into.join("probe-mod");
    std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(root.join("hooks")).unwrap();
    std::fs::write(
        root.join(".claude-plugin/plugin.json"),
        json!({ "name": "probe-mod", "version": "0.1.0", "rebon": {"format":"claude-mods","formatVersion":1,"adapterRevision":1,"sdk":[{"name":"rebon-claude-mods-api","range":"^1"}]} }).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("hooks/hooks.json"),
        json!({ "modules": ["./register.ts"] }).to_string(),
    )
    .unwrap();
    std::fs::write(root.join("hooks/register.ts"), PROBE_MOD).unwrap();
    root
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contained_mod_reads_its_own_writes_its_data_and_asks_for_the_rest() {
    let Some(node) = node() else { return };
    let repo = repo();
    let tmp = tempfile::tempdir().expect("temp dir");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let mod_root = write_probe_mod(tmp.path());

    let kernel = Kernel::new();
    kernel
        .load(vec![
            Box::new(ConfigSeatsPlugin::new(config_dir.clone()).with_cwd(config_dir.clone())),
            Box::new(CoreCommandsPlugin::new(kernel.clone())),
        ])
        .expect("kernel seats load");
    let ctx = kernel.context().fork_scoped("plane");
    let registry = ComposeToolRegistry::new(Vec::<String>::new());
    ctx.provide_json("tool-registry", registry.clone()).unwrap();
    ctx.provide_json(SYSTEM_PROMPT_SERVICE, ComposePromptSections::new())
        .unwrap();
    let tools = Arc::new(RecordingTools::default());
    let plane = PluginPlane::start(
        PluginPlaneConfig {
            node,
            host_script: PathBuf::from(plain(&repo.join("runtimes/node/plugin-host/src/cli.mjs"))),
            loader: PathBuf::from(plain(
                &repo.join("runtimes/node/compose-runtime/src/index.mjs"),
            )),
            compose_root: PathBuf::from(plain(&repo.join("runtimes/node/compose-runtime"))),
            payload_dir: Some(PathBuf::from(plain(
                &repo.join("runtimes/node/compose-runtime/payload"),
            ))),
            structure: Vec::new(),
            web: Value::Null,
            modules: BTreeMap::new(),
            exposed_tools: Vec::new(),
            exposed_seats: default_exposed_seats(),
            tool_catalog: json!([]),
            scope_id: None,
            working_directory: repo.clone(),
            unary_call_timeout: Some(Duration::from_secs(10)),
            drain_deadline: None,
            lifecycle_sink: None,
        },
        ctx.clone(),
        registry,
        tools.clone() as Arc<dyn ToolInvoker>,
    )
    .await
    .expect("the plugin plane starts");
    let mods = plane.install_mods(&kernel, config_dir.clone());

    let outcome = AssertUnwindSafe(async {
        let data = tmp.path().join("data/mod-probe");
        let mut entry = mod_entry("probe-mod", &mod_root, &config_dir, &Value::Null, &[])
            .expect("the folder reads");
        entry.container = Some(ContainerSpec {
            id: "mod-probe".into(),
            read: vec![entry.root.clone()],
            data_dir: plain(&data),
            network: Vec::new(),
            env: Vec::new(),
        });
        plane.load_entry(&entry).await.expect("the mod loads in its container");
        assert_eq!(
            plane.containers().await.get("mod-probe"),
            Some(&vec!["probe-mod".to_string()])
        );

        let ask = |request: Value| {
            let mods = Arc::clone(&mods);
            async move {
                let answer = mods
                    .call_mod(
                        "probe-mod",
                        json!({ "kind": "command", "command": "probe", "args": request.to_string() }),
                    )
                    .await
                    .expect("the mod answers");
                let text = answer
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
            }
        };

        // Its own folder reads; a file elsewhere does not.
        let own = plain(&mod_root.join("hooks/register.ts"));
        assert_eq!(ask(json!({ "do": "read", "path": own })).await["ok"], true);
        let elsewhere = plain(&repo.join("Cargo.toml"));
        let refused = ask(json!({ "do": "read", "path": elsewhere })).await;
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(refused.to_string().contains("NOT_PERMITTED"), "{refused}");

        // Its data directory takes a write directly; nothing was asked.
        let note = data.join("note.txt");
        assert_eq!(ask(json!({ "do": "write", "path": plain(&note) })).await["ok"], true);
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "probe");
        assert!(tools.seen.lock().unwrap().is_empty());

        // A write anywhere else goes to rebon's Write tool instead of the disk.
        let outside = tmp.path().join("outside.txt");
        assert_eq!(ask(json!({ "do": "write", "path": plain(&outside) })).await["ok"], true);
        assert!(!outside.exists(), "the seat did not write it itself");
        // A process goes to rebon's Bash tool.
        let ran = ask(json!({ "do": "run", "argv": ["echo", "hi there"] })).await;
        assert_eq!(ran["value"]["stdout"], "ran through the tool", "{ran}");
        {
            let seen = tools.seen.lock().unwrap();
            assert_eq!(seen[0].0, "Write");
            assert_eq!(seen[0].1["file_path"], plain(&outside));
            assert_eq!(seen[1].0, "Bash");
            assert_eq!(seen[1].1["command"], "echo 'hi there'");
        }

        // No host was granted: the network is refused by name.
        let fetched = ask(json!({ "do": "fetch", "url": "http://localhost:9/" })).await;
        assert_eq!(fetched["ok"], false, "{fetched}");
        assert!(fetched.to_string().contains("NOT_PERMITTED"), "{fetched}");
    })
    .catch_unwind()
    .await;
    plane.shutdown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
