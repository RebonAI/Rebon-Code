//! Kernel-backed seat for skills a compiled feature plugin ships.
//!
//! A skill is a directory — `SKILL.md` plus whatever it references — and the
//! session's skill index loads directories. An installed package already
//! reaches that loader by naming a directory in its manifest; a compiled
//! plugin has no directory of its own on the user's disk, and it cannot hand
//! its skill to the `skill` plugin directly, because plugins do not depend on
//! plugins. It registers a [`SkillBundle`] here instead: the files, embedded
//! in the binary. Whoever builds a session's skill index reads the seat, puts
//! each bundle on disk, and loads it like any plugin skill directory — so a
//! bundled skill gets the base-directory prefix, `${CLAUDE_SKILL_DIR}`, the
//! denylist and `required-tools` exactly as one a package installed.
//!
//! Lifetime. A registration is an effect of the registering context: turning
//! the plugin off takes its bundles off the seat, and the next session index
//! is built without them. An index already built keeps what it loaded; a
//! skill whose `required-tools` left with the plugin stops being listed and
//! refuses to run, which is the half that matters.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebon_kernel::{Context, Disposer, KernelError, Service};

pub const SKILL_SEAT_SERVICE: &str = "skill-bundles";

/// One skill directory, embedded.
///
/// `name` is the directory the files land in and must match the skill's
/// frontmatter name. Every path in `files` is relative to that directory,
/// uses `/`, and one of them is `SKILL.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillBundle {
    pub name: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

impl SkillBundle {
    /// Why this bundle cannot be put on disk, if it cannot: the checks run at
    /// registration so a malformed bundle fails its plugin's load rather than
    /// surfacing as a skill that silently never appears.
    fn validate(&self) -> Result<(), String> {
        if !is_plain_segment(self.name) {
            return Err(format!(
                "skill bundle name {:?} is not one path segment",
                self.name
            ));
        }
        if !self.files.iter().any(|(path, _)| *path == "SKILL.md") {
            return Err(format!("skill bundle {} has no SKILL.md", self.name));
        }
        for (path, _) in self.files {
            if path.is_empty() || !path.split('/').all(is_plain_segment) {
                return Err(format!(
                    "skill bundle {} names {path:?}, which is not a relative path",
                    self.name
                ));
            }
        }
        Ok(())
    }
}

fn is_plain_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(['/', '\\', ':'])
}

/// Typed definition for the kernel's `skill-bundles` seat.
pub struct SkillSeatService;

impl Service for SkillSeatService {
    type Interface = SkillSeat;
    const NAME: &'static str = SKILL_SEAT_SERVICE;
}

struct BundleEntry {
    token: u64,
    bundle: SkillBundle,
}

/// Registry behind the typed `skill-bundles` service.
pub struct SkillSeat {
    entries: RwLock<Vec<BundleEntry>>,
    next_token: AtomicU64,
}

impl SkillSeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(1),
        })
    }

    /// Register `bundle` for `provider` on `ctx`. The registration is an
    /// effect of the context and leaves the seat when `ctx` is disposed. A
    /// second bundle with the same name is a conflict: two plugins would be
    /// writing one directory.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Context,
        provider: &str,
        bundle: SkillBundle,
    ) -> Result<(), KernelError> {
        bundle.validate().map_err(KernelError::Other)?;
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut entries = self.entries.write().expect("skill seat poisoned");
            if entries.iter().any(|entry| entry.bundle.name == bundle.name) {
                return Err(KernelError::DuplicateProvider {
                    plugin: provider.to_string(),
                    service: format!("{SKILL_SEAT_SERVICE}:{}", bundle.name),
                });
            }
            entries.push(BundleEntry { token, bundle });
        }

        let weak = Arc::downgrade(self);
        ctx.effect_labeled(&format!("skill bundle({})", bundle.name), move || {
            Disposer::new(move || {
                if let Some(seat) = weak.upgrade() {
                    seat.entries
                        .write()
                        .expect("skill seat poisoned")
                        .retain(|entry| entry.token != token);
                }
            })
        });
        Ok(())
    }

    /// Every registered bundle, ordered by name.
    pub fn bundles(&self) -> Vec<SkillBundle> {
        let mut bundles: Vec<SkillBundle> = self
            .entries
            .read()
            .expect("skill seat poisoned")
            .iter()
            .map(|entry| entry.bundle)
            .collect();
        bundles.sort_by(|left, right| left.name.cmp(right.name));
        bundles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    const SKILL: SkillBundle = SkillBundle {
        name: "imagegen",
        files: &[
            ("SKILL.md", "---\nname: imagegen\n---\nbody"),
            ("references/a.md", "a"),
        ],
    };

    fn bundle(name: &'static str, files: &'static [(&'static str, &'static str)]) -> SkillBundle {
        SkillBundle { name, files }
    }

    #[test]
    fn a_registered_bundle_is_listed_until_its_context_is_disposed() {
        let kernel = Kernel::new();
        let seat = SkillSeat::new();
        let plugin = kernel.context().fork_scoped("image-gen");
        seat.register(&plugin, "image-gen", SKILL)
            .expect("valid bundle");
        assert_eq!(seat.bundles(), vec![SKILL]);

        plugin.dispose();
        assert!(seat.bundles().is_empty());
    }

    #[test]
    fn two_bundles_may_not_share_a_directory() {
        let kernel = Kernel::new();
        let seat = SkillSeat::new();
        let ctx = kernel.context();
        seat.register(&ctx, "one", SKILL).expect("first");
        let err = seat.register(&ctx, "two", SKILL).unwrap_err();
        assert!(
            matches!(err, KernelError::DuplicateProvider { .. }),
            "{err:?}"
        );
        assert_eq!(seat.bundles().len(), 1);
    }

    #[test]
    fn bundles_are_listed_by_name() {
        let kernel = Kernel::new();
        let seat = SkillSeat::new();
        let ctx = kernel.context();
        seat.register(&ctx, "p", bundle("zeta", &[("SKILL.md", "")]))
            .expect("zeta");
        seat.register(&ctx, "p", bundle("alpha", &[("SKILL.md", "")]))
            .expect("alpha");
        let names: Vec<&str> = seat.bundles().iter().map(|b| b.name).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn a_bundle_that_cannot_be_put_on_disk_is_refused_at_registration() {
        let kernel = Kernel::new();
        let seat = SkillSeat::new();
        let ctx = kernel.context();
        for bad in [
            bundle("", &[("SKILL.md", "")]),
            bundle("..", &[("SKILL.md", "")]),
            bundle("a/b", &[("SKILL.md", "")]),
            bundle("no-skill-md", &[("README.md", "")]),
            bundle("escapes", &[("SKILL.md", ""), ("../x.md", "")]),
            bundle("absolute", &[("SKILL.md", ""), ("C:/x.md", "")]),
            bundle("backslash", &[("SKILL.md", ""), ("a\\b.md", "")]),
            bundle("empty-segment", &[("SKILL.md", ""), ("a//b.md", "")]),
        ] {
            assert!(
                seat.register(&ctx, "p", bad).is_err(),
                "{:?} should be refused",
                bad.name
            );
        }
        assert!(seat.bundles().is_empty());
    }
}
