//! A Claude Code mod on the real plane, end to end.
//!
//! The mod is the `mods-runtime` package's own fixture (`counter-mod`): a
//! `.tsx` hooks module that registers a command at `session.start`, sets a
//! status, rewrites and denies `tool.call`, rewrites `prompt.submit`, draws a
//! pane with a button, and answers `classic.Stop`. What this asserts is the
//! rebon half of each: the composition reads the folder into a load request,
//! the plane loads it through the compose loader, the mods registry seats its
//! command on the command seat and refines it when `$.command.register` runs,
//! `$.ui.status` lands in the UI table, a render comes back as a validated
//! tree, a press runs the handler and the next render shows it, and the
//! policy subscriber turns the mod's hook answers into the effects a settings
//! hook would produce.
//!
//! Skips without `REBON_TEST_NODE`, like every test that drives a real child
//! process; `REBON_REQUIRE_TEST_NODE=1` turns the skip into a failure. Run
//! serially: it shares the process-wide tool slot with the other plane tests.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt as _;

use rebon_command_seat::{CommandArgs, CommandHandler, CommandSeatService, Surface};
use rebon_core::policy_seat::{PolicyRequest, PolicySubscriber, Verdict};
use rebon_hooks::{HookEffect, HookEventPayload, HookInvocationContext};
use rebon_kernel::Kernel;
use rebon_kernel_seats::kernel_compose_tools::ComposeToolRegistry;
use rebon_kernel_seats::kernel_config_seats::ConfigSeatsPlugin;
use rebon_kernel_seats::kernel_core_commands::CoreCommandsPlugin;
use rebon_kernel_seats::kernel_prompt_sections::{ComposePromptSections, SYSTEM_PROMPT_SERVICE};
use rebon_plugin_host::mods::compose::mod_entry;
use rebon_plugin_host::mods::policy::ModsPolicySubscriber;
use rebon_plugin_host::mods::{ModRenderAnswer, ModsLink};
use rebon_plugin_host::plugin_plane::{
    default_exposed_seats, ComposeNode, PluginPlane, PluginPlaneConfig,
};
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::{ToolInvocation, ToolInvoker, ToolRefusal};
use rebon_types::ModUiSurface;
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

/// Windows verbatim prefixes are not paths the protocol's own rule accepts.
fn plain(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
}

struct NoTools;

impl ToolInvoker for NoTools {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        Box::pin(async move {
            Err(ToolRefusal::new(
                "[NO_TOOLS]",
                format!("this fixture serves no tool ({})", invocation.tool),
            ))
        })
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("fixture dir");
    for entry in std::fs::read_dir(from).expect("fixture readable") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("fixture file copies");
        }
    }
}

/// The fixture mod, copied under a temporary folder so the test owns it.
fn counter_mod(into: &Path) -> PathBuf {
    let source = repo().join("runtimes/node/mods-runtime/test/fixtures/counter-mod");
    let root = into.join("counter-mod");
    copy_dir(&source, &root);
    root
}

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{what} did not happen within five seconds");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mod_loads_seats_its_command_draws_and_answers_hooks() {
    let Some(node) = node() else { return };
    let repo = repo();
    let tmp = tempfile::tempdir().expect("temp dir");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("settings.json"),
        json!({ "plugins": { "counter": { "prefix": "n=" } } }).to_string(),
    )
    .expect("settings written");
    let mod_root = counter_mod(tmp.path());

    let kernel = Kernel::new();
    kernel
        .load(vec![
            Box::new(ConfigSeatsPlugin::new(config_dir.clone()).with_cwd(config_dir.clone())),
            Box::new(CoreCommandsPlugin::new(kernel.clone())),
        ])
        .expect("kernel seats load");
    let ctx = kernel.context().fork_scoped("plane");
    let registry = ComposeToolRegistry::new(Vec::<String>::new());
    ctx.provide_json("tool-registry", registry.clone())
        .expect("the composition tool seat provides");
    ctx.provide_json(SYSTEM_PROMPT_SERVICE, ComposePromptSections::new())
        .expect("the prompt section seat provides");

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
            structure: vec![ComposeNode {
                id: "counter".into(),
                ..Default::default()
            }],
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
        Arc::new(NoTools) as Arc<dyn ToolInvoker>,
    )
    .await
    .expect("the plugin plane starts");
    let mods = plane.install_mods(&kernel, config_dir.clone());

    // The host stays up only as long as this test does: an assertion that
    // fails must still shut the plane down, or the Node child keeps the test
    // binary alive and nothing reports the failure.
    let outcome = AssertUnwindSafe(scenario(
        &plane,
        &mods,
        &kernel,
        &mod_root,
        &config_dir,
        tmp.path(),
    ))
    .catch_unwind()
    .await;
    plane.shutdown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn scenario(
    plane: &Arc<PluginPlane>,
    mods: &Arc<rebon_plugin_host::mods::ModsRegistry>,
    kernel: &Arc<Kernel>,
    mod_root: &Path,
    config_dir: &Path,
    tmp: &Path,
) {
    // The composition reads the folder: the scanned ceiling, the marker, the
    // stored option.
    let entry =
        mod_entry("counter", mod_root, config_dir, &Value::Null, &[]).expect("the folder reads");
    assert_eq!(entry.commands, vec!["count"]);
    assert_eq!(entry.config["$claudeMod"]["options"]["prefix"], json!("n="));

    plane.load_entry(&entry).await.expect("the mod loads");
    assert!(
        mods.get("counter").is_some(),
        "the registry attached the mod"
    );

    // `session.start` ran when the scope opened: `$.ui.status` landed, and
    // `$.command.register` refined the command's description on the seat.
    wait_for("the status line", || {
        mods.ui.snapshot().status == vec![("counter".to_owned(), "n=0".to_owned())]
    })
    .await;
    let seat = kernel
        .context()
        .get::<CommandSeatService>()
        .expect("the command seat");
    let command = seat.find("count").expect("/count is on the seat");
    assert_eq!(command.spec.description, "Shows the count");
    assert_eq!(command.spec.hint.as_deref(), Some("[reset]"));
    assert!(command.owner.contains("node/counter"), "{}", command.owner);
    let CommandHandler::Prompt(expand) = &command.handler else {
        panic!("a mod's command is a prompt command");
    };
    let expanded = tokio::task::spawn_blocking({
        let expand = Arc::clone(expand);
        move || {
            expand(&CommandArgs {
                raw: "/count".into(),
                rest: String::new(),
                surface: Surface::Tui,
            })
        }
    })
    .await
    .expect("the proxy thread joins")
    .expect("the command expands");
    assert_eq!(expanded, "count is 0");

    // A render is a validated tree; a press runs the handler; the next
    // render shows the new state, and the pane's version moved.
    let first = mods
        .render(
            "counter",
            "Pane",
            ModUiSurface::Terminal,
            "counter",
            json!({ "bodyColumns": 40 }),
            Some((80, 24)),
        )
        .await
        .expect("the mod draws");
    let ModRenderAnswer::Tree(tree) = first else {
        panic!("the Pane hook draws a tree");
    };
    assert_eq!(tree.ty, "Box");
    assert_eq!(tree.children.len(), 2);
    let mut buttons = Vec::new();
    tree.find_all("Button", &mut buttons);
    assert_eq!(buttons[0].key(), Some("more"));
    mods.press(
        "counter",
        "Pane",
        ModUiSurface::Terminal,
        "counter",
        "more",
        None,
    )
    .await
    .expect("the press is delivered");
    let second = mods
        .render(
            "counter",
            "Pane",
            ModUiSurface::Terminal,
            "counter",
            json!({}),
            None,
        )
        .await
        .expect("the mod draws again");
    let ModRenderAnswer::Tree(tree) = second else {
        panic!("a tree again");
    };
    let mut texts = Vec::new();
    tree.find_all("Text", &mut texts);
    assert_eq!(texts[0].text(), "1 click");
    let elsewhere = mods
        .render(
            "counter",
            "Pane",
            ModUiSurface::Desktop,
            "other",
            json!({}),
            None,
        )
        .await
        .expect("an unmatched instance is answered");
    assert_eq!(elsewhere, ModRenderAnswer::Engine);

    // The policy subscriber: a denied Bash, a rewritten one whose result
    // arrives later, a rewritten prompt, a Stop message.
    let subscriber = ModsPolicySubscriber::new(mods);
    let context = HookInvocationContext {
        cwd: plain(tmp),
        transcript_path: plain(&tmp.join("t.jsonl")),
        session_id: "s1".into(),
        permission_mode: None,
        agent_id: None,
        agent_type: None,
    };
    let denied = subscriber
        .decide(&PolicyRequest::new(
            context.clone(),
            HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: json!({ "command": "rm -rf /" }),
                tool_use_id: "t1".into(),
            },
        ))
        .await;
    assert!(
        matches!(&denied, Verdict::Modify { effects } if matches!(&effects[0], HookEffect::BlockToolCall { reason, .. } if reason == "not on my watch")),
        "{denied:?}"
    );
    let rewritten = subscriber
        .decide(&PolicyRequest::new(
            context.clone(),
            HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: json!({ "command": "ls" }),
                tool_use_id: "t2".into(),
            },
        ))
        .await;
    assert!(
        matches!(&rewritten, Verdict::Modify { effects } if effects.iter().any(|e| matches!(e, HookEffect::UpdateToolInput { input } if input["command"] == json!("ls # seen")))),
        "{rewritten:?}"
    );
    let after = subscriber
        .decide(&PolicyRequest::new(
            context.clone(),
            HookEventPayload::PostToolUse {
                tool_name: "Bash".into(),
                tool_input: json!({ "command": "ls # seen" }),
                tool_response: json!({ "stdout": "a\n" }),
                tool_use_id: "t2".into(),
            },
        ))
        .await;
    assert!(
        matches!(&after, Verdict::Modify { effects } if effects.iter().any(|e| matches!(e, HookEffect::InjectContext { text } if text == "bash answered fine"))),
        "{after:?}"
    );
    let prompt = subscriber
        .decide(&PolicyRequest::new(
            context.clone(),
            HookEventPayload::UserPromptSubmit {
                prompt: "hello".into(),
            },
        ))
        .await;
    assert!(
        matches!(&prompt, Verdict::Modify { effects } if effects.iter().any(|e| matches!(e, HookEffect::ReplacePrompt { text } if text == "HELLO"))),
        "{prompt:?}"
    );
    let stop = subscriber
        .decide(&PolicyRequest::new(
            context.clone(),
            HookEventPayload::Stop {
                stop_reason: Some("end_turn".into()),
                last_assistant_message: Some("the answer".into()),
                stop_hook_active: false,
            },
        ))
        .await;
    assert!(
        matches!(&stop, Verdict::Modify { effects } if effects.iter().any(|e| matches!(e, HookEffect::SystemMessage { text } if text.contains("seen by counter") || text.contains("stopped")))),
        "{stop:?}"
    );
    // An event the mod does not listen to costs no call and no opinion.
    assert!(!subscriber.interest(rebon_hooks::HookEvent::Notification));

    // The same questions through the link a surface in another process
    // uses: one JSON call per question, answered by `serve_remote`.
    let link = ModsLink::Local(Arc::clone(mods));
    let view = link
        .snapshot(None, true, ModUiSurface::Desktop)
        .await
        .expect("a snapshot");
    assert!(view.changed);
    assert_eq!(view.commands.len(), 1);
    assert_eq!(view.commands[0].name, "count");
    assert_eq!(view.commands[0].plugin_name, "counter");
    assert_eq!(view.mods[0]["name"], json!("counter"));
    let at_rest = link
        .snapshot(Some(view.version), true, ModUiSurface::Desktop)
        .await
        .expect("a second snapshot");
    assert!(
        !at_rest.changed,
        "nothing moved, and an empty take moves nothing"
    );
    link.press(
        "counter",
        "Pane",
        ModUiSurface::Desktop,
        "counter",
        "more",
        None,
    )
    .await
    .expect("a press through the link");
    let ModRenderAnswer::Tree(tree) = link
        .render(
            "counter",
            "Pane",
            ModUiSurface::Desktop,
            "counter",
            json!({}),
            Some((48, 40)),
        )
        .await
        .expect("a drawing through the link")
    else {
        panic!("a tree through the link");
    };
    let mut texts = Vec::new();
    tree.find_all("Text", &mut texts);
    assert_eq!(texts[0].text(), "2 clicks");
    let answer = link
        .command("count", "", ModUiSurface::Desktop)
        .await
        .expect("the command runs where the mod lives");
    assert_eq!(answer.text, "count is 2");
    assert_eq!(answer.plugin_name, "counter");
    let unknown = link.command("nope", "", ModUiSurface::Desktop).await;
    assert!(unknown.is_err());

    // Unloading takes the command off the seat and the mod out of the table.
    assert!(
        plane.unload_entry("counter").await.is_clean(),
        "the mod unloads"
    );
    assert!(mods.get("counter").is_none());
    assert!(seat.find("count").is_none(), "/count left with the mod");
    assert!(mods.ui.snapshot().status.is_empty());
}
