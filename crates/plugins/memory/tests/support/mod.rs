//! Shared scaffolding for this crate's integration tests: a kernel with the
//! four root seats the plugin injects, and a temp home so the memory
//! directories belong to the test rather than to whoever runs it.
//!
//! One copy rather than one per test binary: the seat list is the plugin's
//! `inject` list, and three copies of it would drift the moment the plugin
//! asks for a fifth seat.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rebon_command_seat::{CommandSeat, CommandSeatService, COMMAND_SEAT_SERVICE};
use rebon_core::attachment_seat::{AttachmentSeat, AttachmentSeatService, ATTACHMENT_SEAT_SERVICE};
use rebon_core::prompt_seat::{PromptSeat, PromptSeatService, PROMPT_SEAT_SERVICE};
use rebon_core::tool_seat::{ToolSeat, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_core::turn_hook::{TurnHookSeat, TurnHookSeatService, TURN_HOOK_SEAT_SERVICE};
use rebon_kernel::{
    Context, DesiredSet, Kernel, KernelError, Plugin, PluginDef, PluginHost, PluginKind,
    PluginMeta, PluginRegistry,
};

/// Stands in for `core-tools` and `core-commands`, which this crate cannot
/// depend on. It provides exactly the seats the plugin injects, plus the
/// command seat `/memory` registers on.
struct SeatPlugin;

impl Plugin for SeatPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("test-seat").provides(&[
            TOOL_SEAT_SERVICE,
            ATTACHMENT_SEAT_SERVICE,
            PROMPT_SEAT_SERVICE,
            TURN_HOOK_SEAT_SERVICE,
            COMMAND_SEAT_SERVICE,
        ])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())?;
        ctx.provide::<PromptSeatService>(PromptSeat::new())?;
        ctx.provide::<TurnHookSeatService>(TurnHookSeat::new())?;
        ctx.provide::<CommandSeatService>(CommandSeat::new())?;
        ctx.provide::<ToolSeatService>(ToolSeat::new())
    }
}

fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(SeatPlugin))
}

static DEFS: &[PluginDef] = &[
    PluginDef {
        id: "test-seat",
        title: "Test seat",
        kind: PluginKind::Core,
        default_enabled: true,
        factory: make_seat,
    },
    rebon_plugin_memory::PLUGIN,
];

/// A kernel with the seats up and the memory plugin loaded.
pub struct Booted {
    pub kernel: Arc<Kernel>,
    pub registry: Arc<PluginRegistry>,
}

impl Booted {
    pub fn prompt_seat(&self) -> Arc<PromptSeat> {
        self.kernel
            .context()
            .get::<PromptSeatService>()
            .expect("the prompt seat is on the root")
    }

    pub fn turn_hook_seat(&self) -> Arc<TurnHookSeat> {
        self.kernel
            .context()
            .get::<TurnHookSeatService>()
            .expect("the turn-hook seat is on the root")
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.registry
            .set_enabled(rebon_plugin_memory::PLUGIN_ID, enabled)
            .expect("memory is a feature plugin");
    }
}

pub fn boot() -> Booted {
    let kernel = Kernel::new();
    let host = PluginHost {
        kernel: kernel.clone(),
        config_dir: std::env::temp_dir(),
    };
    let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
    let report = registry.reconcile(&DesiredSet::new());
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    Booted { kernel, registry }
}

/// One lock for the tests that rewrite `HOME` / `USERPROFILE` /
/// `REBON_CONFIG_DIR`, which are process-global.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A temp home, held for the length of one test.
pub struct HomeGuard {
    _temp: tempfile::TempDir,
    pub home: PathBuf,
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl HomeGuard {
    pub fn new() -> Self {
        let _lock = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let temp = tempfile::tempdir().expect("temp home");
        let home = temp.path().to_path_buf();
        let names = [
            "HOME",
            "USERPROFILE",
            "REBON_CONFIG_DIR",
            "REBON_DISABLE_AUTO_MEMORY",
            "REBON_SIMPLE",
        ];
        let prev = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("REBON_CONFIG_DIR", home.join(".rebon"));
        std::env::remove_var("REBON_DISABLE_AUTO_MEMORY");
        std::env::remove_var("REBON_SIMPLE");
        Self {
            _temp: temp,
            home,
            prev,
            _lock,
        }
    }

    /// The config home every lookup resolves to under this guard.
    pub fn config_home(&self) -> PathBuf {
        self.home.join(".rebon")
    }

    /// An isolated project directory.
    pub fn cwd(&self, label: &str) -> String {
        let cwd = self.home.join("proj").join(label);
        std::fs::create_dir_all(&cwd).expect("project dir");
        cwd.to_string_lossy().into_owned()
    }

    /// A path inside the repo-scope memory directory for `cwd`, whether or
    /// not anything is written there.
    pub fn memory_path(&self, cwd: &str, name: &str) -> PathBuf {
        rebon_session::memory_paths::repo_memory_dir(cwd)
            .expect("memory dir")
            .join(name)
    }

    /// Write `body` to the repo-scope `MEMORY.md` for `cwd`.
    pub fn seed_memory_md(&self, cwd: &str, body: &str) -> PathBuf {
        let path = self.memory_path(cwd, "MEMORY.md");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("memory dir");
        std::fs::write(&path, body).expect("MEMORY.md");
        path
    }

    /// Write `body` to the project `REBON.md` for `cwd`, returning the
    /// canonical path the read-state cache keys on.
    pub fn seed_project_rebon_md(&self, cwd: &str, body: &str) -> PathBuf {
        let path = PathBuf::from(cwd).join("REBON.md");
        std::fs::write(&path, body).expect("REBON.md");
        std::fs::canonicalize(path).expect("canonical REBON.md")
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        for (name, value) in std::mem::take(&mut self.prev) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
