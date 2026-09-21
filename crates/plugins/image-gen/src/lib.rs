//! `image-gen`: the feature plugin that puts the `ImageGen` tool on the process
//! tool seat and the `imagegen` skill on the skill-bundles seat.
//!
//! The tool calls the OpenAI Images API with the session provider's own base
//! URL and credentials, so it exists for the model only while the adopted
//! provider is a first-party OpenAI route — `api.openai.com` with a key, or
//! the ChatGPT Codex route with OAuth. `rebon-harness` decides that when it
//! resolves the provider and publishes the endpoint through
//! [`set_provider_endpoint`]. The skill is the prompting guidance for that
//! tool, taken from codex's `imagegen` system skill; it declares
//! `required-tools: ImageGen`, so the listing names it only in a turn that
//! offers the tool.
//!
//! Turning the plugin off takes both away. A session index already loaded
//! keeps the skill entry, but with the tool gone it is neither listed nor
//! runnable.

use std::sync::Arc;

use rebon_core::skill_seat::{SkillBundle, SkillSeatService, SKILL_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

mod endpoint;
mod tool;

pub use endpoint::{set_provider_endpoint, ImagesEndpoint};
pub use tool::{ImageGenTool, IMAGE_GEN_TOOL_NAME};

/// Stable id: the config key `plugins.image-gen.enabled`.
pub const PLUGIN_ID: &str = "image-gen";

const PROVIDER_ID: &str = "image-gen";

/// The `imagegen` skill directory, embedded. Paths mirror
/// `skill/imagegen/` in this crate.
pub const IMAGEGEN_SKILL: SkillBundle = SkillBundle {
    name: "imagegen",
    files: &[
        ("SKILL.md", include_str!("../skill/imagegen/SKILL.md")),
        ("LICENSE.txt", include_str!("../skill/imagegen/LICENSE.txt")),
        (
            "references/prompting.md",
            include_str!("../skill/imagegen/references/prompting.md"),
        ),
        (
            "references/sample-prompts.md",
            include_str!("../skill/imagegen/references/sample-prompts.md"),
        ),
        (
            "references/cli.md",
            include_str!("../skill/imagegen/references/cli.md"),
        ),
        (
            "references/image-api.md",
            include_str!("../skill/imagegen/references/image-api.md"),
        ),
        (
            "scripts/image_gen.py",
            include_str!("../skill/imagegen/scripts/image_gen.py"),
        ),
        (
            "scripts/remove_chroma_key.py",
            include_str!("../skill/imagegen/scripts/remove_chroma_key.py"),
        ),
    ],
};

/// The one tool, for a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(ImageGenTool) as Arc<dyn rebon_tool::Tool>]
}

pub struct ImageGenPlugin;

impl Plugin for ImageGenPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE, SKILL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let tools = ctx.require::<ToolSeatService>()?;
        tools.register_tools(ctx, PROVIDER_ID, Priority::Feature, self::tools())?;
        let skills = ctx.require::<SkillSeatService>()?;
        skills.register(ctx, PROVIDER_ID, IMAGEGEN_SKILL)
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(ImageGenPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Image generation (ImageGen, imagegen skill)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::skill_seat::SkillSeat;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::ToolResolver;

    /// Stands in for `core-tools`, which lives in `rebon-kernel-seats` and
    /// cannot be depended on from here. All this plugin needs is the two
    /// root seats.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[TOOL_SEAT_SERVICE, SKILL_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<ToolSeatService>(ToolSeat::new())?;
            ctx.provide::<SkillSeatService>(SkillSeat::new())
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
        PLUGIN,
    ];

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    fn registered(kernel: &Kernel) -> (bool, bool) {
        let ctx = kernel.context();
        let tool = ctx
            .get::<ToolSeatService>()
            .expect("tool seat")
            .resolve(IMAGE_GEN_TOOL_NAME, None)
            .expect("resolve")
            .is_some();
        let skill = ctx
            .get::<SkillSeatService>()
            .expect("skill seat")
            .bundles()
            .contains(&IMAGEGEN_SKILL);
        (tool, skill)
    }

    #[test]
    fn the_switch_takes_the_tool_and_the_skill_off_their_seats_and_puts_them_back() {
        // The seat only resolves an enabled tool, and the tool is enabled
        // only while an endpoint is published.
        let _lock = endpoint::endpoint_test_lock();
        set_provider_endpoint(Some(Arc::new(ImagesEndpoint::new(
            "https://api.openai.com/v1",
            "sk",
            None,
        ))));
        let (kernel, registry) = boot();
        assert_eq!(registered(&kernel), (true, true));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("image-gen is a feature plugin");
        assert_eq!(registered(&kernel), (false, false));

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(registered(&kernel), (true, true));

        // A provider without an endpoint leaves the tool registered but off
        // the model's list; the skill stays on its seat and `required-tools`
        // keeps it out of the listing.
        set_provider_endpoint(None);
        assert_eq!(registered(&kernel), (false, true));
    }

    #[test]
    fn a_kernel_without_the_seats_refuses_the_plugin() {
        static ALONE: &[PluginDef] = &[PLUGIN];
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), ALONE, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(
            !report.failed.is_empty(),
            "image-gen must not load without its seats"
        );
    }

    /// Every file under `skill/imagegen/` is in the bundle, and the bundle
    /// names nothing that is not there — a reference added on disk but not
    /// here would be a link the model follows to nothing.
    #[test]
    fn the_bundle_carries_exactly_the_skill_directory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("skill")
            .join("imagegen");
        let mut on_disk = Vec::new();
        collect_files(&root, &root, &mut on_disk);
        on_disk.sort();
        let mut bundled: Vec<String> = IMAGEGEN_SKILL
            .files
            .iter()
            .map(|(path, _)| path.to_string())
            .collect();
        bundled.sort();
        assert_eq!(bundled, on_disk);
    }

    fn collect_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("skill dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                collect_files(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).expect("under root");
                out.push(
                    relative
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
    }

    /// The skill teaches this crate's tool: its frontmatter must tie it to
    /// the tool by name, and the body must call the tool what it is called.
    #[test]
    fn the_skill_is_bound_to_the_tool_it_teaches() {
        let (_, skill_md) = IMAGEGEN_SKILL
            .files
            .iter()
            .find(|(path, _)| *path == "SKILL.md")
            .expect("SKILL.md");
        let frontmatter = skill_md
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(frontmatter, _)| frontmatter)
            .expect("frontmatter");
        assert!(
            frontmatter
                .lines()
                .any(|line| line.trim() == format!("required-tools: {IMAGE_GEN_TOOL_NAME}")),
            "{frontmatter}"
        );
        assert!(skill_md.contains(&format!("`{IMAGE_GEN_TOOL_NAME}`")));
        for (path, contents) in IMAGEGEN_SKILL.files {
            for stale in [
                "`image_gen`",
                "view_image",
                "CODEX_HOME",
                "codex-network.md",
            ] {
                assert!(!contents.contains(stale), "{path} still says {stale}");
            }
        }
    }
}
