//! Lazy on-disk extraction of compile-time-embedded bundled skills.
//!
//! The lifecycle:
//!
//! 1. **Startup** — built-in skill definitions (embedded via
//!    `include_str!` in [`crate::skills::built_in`]) are registered into
//!    the [`SkillRegistry`] so they are immediately invocable.
//! 2. **First invocation** — the SKILL.md content is lazily written
//!    to `<config_home>/.cache/bundled-skills/<nonce>/<name>/SKILL.md`
//!    so the model can `Read`/`Grep` the file on demand.
//! 3. **Subsequent invocations** — the extraction is memoized
//!    per-process; the cached path is returned immediately.
//!
//! Security: the per-process random nonce prevents symlink squatting.
//! Files are created with `O_EXCL` (no clobber) and restrictive
//! permissions (0o700 dirs, 0o600 files on Unix).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::skill::{Skill, SkillRegistry, SkillSource};
use crate::skills::{built_in_skills, bundled_skill_to_entry, BundledSkillDefinition};

// ---------------------------------------------------------------------------
// Nonce — per-process random directory component
// ---------------------------------------------------------------------------

/// Returns a per-process random hex string (32 hex chars / 16 bytes).
fn process_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        // Mix PID + high-resolution time for uniqueness.
        // Not cryptographic — the directory-permission model is the
        // real defense (0o700). The nonce just prevents name guessing.
        let pid = std::process::id();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{pid:08x}{ts:024x}")
    })
}

/// Resolve `~/.rebon` (or `$REBON_CONFIG_DIR`), the way the rest of the
/// process does.
fn config_home() -> PathBuf {
    rebon_session::default_config_home_dir()
}

/// Root directory for bundled skill extraction:
/// `<config_home>/.cache/bundled-skills/<nonce>`.
fn bundled_skills_root() -> PathBuf {
    let mut p = config_home();
    p.push(".cache");
    p.push("bundled-skills");
    p.push(process_nonce());
    p
}

// ---------------------------------------------------------------------------
// Lazy extraction state
// ---------------------------------------------------------------------------

/// Per-process extraction state: tracks which skills have been
/// extracted and their on-disk paths.
struct ExtractionState {
    /// skill name → on-disk directory (or `None` if extraction failed).
    extracted: HashMap<String, Option<PathBuf>>,
}

static EXTRACTION_STATE: OnceLock<Mutex<ExtractionState>> = OnceLock::new();

fn extraction_state() -> &'static Mutex<ExtractionState> {
    EXTRACTION_STATE.get_or_init(|| {
        Mutex::new(ExtractionState {
            extracted: HashMap::new(),
        })
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Register all compile-time-embedded bundled skills into the
/// runtime [`SkillRegistry`].
///
/// Call this at engine startup, before the first model turn.
/// The skills are immediately invocable — disk extraction is
/// deferred to [`extract_bundled_skill`].
pub fn register_built_in_skills(registry: &SkillRegistry) {
    for def in built_in_skills() {
        let entry = bundled_skill_to_entry(&def);
        registry.register(Skill {
            id: entry.id,
            title: entry.title,
            description: entry.description,
            prompt_template: entry.prompt_template,
            suggested_tools: entry.suggested_tools,
            source: SkillSource::BuiltIn,
            argument_hint: def.argument_hint.clone(),
            argument_names: Vec::new(),
            skill_root: if def.files.is_empty() {
                None
            } else {
                Some(cache_root().join(&def.name).display().to_string())
            },
            user_invocable: def.user_invocable,
            disable_model_invocation: def.disable_model_invocation,
            required_tools: Vec::new(),
        });
    }
    tracing::debug!(
        count = built_in_skills().len(),
        "bundled skill cache: registered built-in skills"
    );
}

/// Lazily extract a bundled skill's SKILL.md to disk so the model
/// can `Read`/`Grep` it.
///
/// Returns the extraction directory path on success, or `None` if
/// the skill is unknown or extraction failed. The result is memoized
/// per-process.
pub async fn extract_bundled_skill(name: &str) -> Option<PathBuf> {
    // Fast path: already extracted (or already failed).
    {
        let guard = extraction_state().lock().expect("extraction state");
        if let Some(cached) = guard.extracted.get(name) {
            return cached.clone();
        }
    }

    // Find the definition among built-in skills.
    let def = built_in_skills().into_iter().find(|d| d.name == name)?;

    // Perform the extraction (async I/O).
    let dir = bundled_skills_root().join(name);
    let result = do_extract(&dir, &def).await;

    // Cache the result.
    let path = match result {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::debug!(
                skill = name,
                error = %e,
                "bundled skill cache: extraction failed"
            );
            None
        }
    };

    let ret = path.clone();
    {
        let mut guard = extraction_state().lock().expect("extraction state");
        guard.extracted.insert(name.to_string(), path);
    }
    ret
}

/// Return the cache directory for a skill if it has already been
/// extracted. Does NOT trigger extraction.
pub fn get_extracted_path(name: &str) -> Option<PathBuf> {
    let guard = extraction_state().lock().expect("extraction state");
    guard.extracted.get(name).and_then(|p| p.clone())
}

/// Return the root directory where bundled skills are extracted.
/// Useful for permission allowlisting.
pub fn cache_root() -> PathBuf {
    bundled_skills_root()
}

// ---------------------------------------------------------------------------
// Extraction I/O
// ---------------------------------------------------------------------------

async fn do_extract(dir: &Path, def: &BundledSkillDefinition) -> std::io::Result<()> {
    // Create the skill directory: <root>/<name>/
    tokio::fs::create_dir_all(dir).await?;

    // On Unix, tighten directory permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        tokio::fs::set_permissions(dir, perms).await?;
    }

    // Write the SKILL.md file.
    let skill_file = dir.join("SKILL.md");
    safe_write_file(&skill_file, &def.prompt_body).await?;

    // Write any additional embedded files.
    for (rel_path, content) in &def.files {
        // Validate: no path traversal.
        if rel_path.contains("..") || Path::new(rel_path).is_absolute() {
            tracing::warn!(
                skill = def.name,
                path = rel_path,
                "bundled skill cache: skipping path with traversal"
            );
            continue;
        }
        let target = dir.join(rel_path);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        safe_write_file(&target, content).await?;
    }

    tracing::debug!(
        skill = def.name,
        dir = %dir.display(),
        "bundled skill cache: extracted to disk"
    );

    Ok(())
}

/// Write a file with `O_EXCL` semantics (fail if it already exists).
/// On Unix, sets mode 0o600. On Windows, uses "wx" flags.
async fn safe_write_file(path: &Path, content: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    // Use OpenOptions with create_new (O_EXCL) to prevent clobbering.
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts.open(path).await?;
    file.write_all(content.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Prepend base directory helper
// ---------------------------------------------------------------------------

/// Prepend a `"Base directory for this skill: <dir>"` header to a
/// prompt string.
pub fn prepend_base_dir(prompt: &str, dir: &Path) -> String {
    format!(
        "Base directory for this skill: {}\n\n{}",
        dir.display(),
        prompt
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_nonce_is_stable_within_process() {
        let a = process_nonce();
        let b = process_nonce();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn bundled_skills_root_contains_nonce() {
        let root = bundled_skills_root();
        let root_str = root.to_string_lossy();
        assert!(root_str.contains(".cache"));
        assert!(root_str.contains("bundled-skills"));
        assert!(root_str.contains(process_nonce()));
    }

    #[test]
    fn register_populates_skill_registry() {
        let registry = SkillRegistry::new();
        register_built_in_skills(&registry);
        assert!(!registry.is_empty());
        // Verify at least simplify is present.
        assert!(registry.get("simplify").is_some());
        assert!(registry.get("batch").is_some());
    }

    #[test]
    fn prepend_base_dir_works() {
        let result = prepend_base_dir("Hello", Path::new("/tmp/skill"));
        assert_eq!(result, "Base directory for this skill: /tmp/skill\n\nHello");
    }

    #[tokio::test]
    async fn extract_and_read_back() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-test-extract-")
            .tempdir()
            .unwrap();
        let tmp = tmp_dir.path();

        let def = BundledSkillDefinition {
            name: "test-skill".into(),
            description: "Test".into(),
            prompt_body: "# Test Prompt\nHello world.".into(),
            ..Default::default()
        };

        do_extract(tmp, &def).await.unwrap();

        let content = tokio::fs::read_to_string(tmp.join("SKILL.md"))
            .await
            .unwrap();
        assert_eq!(content, "# Test Prompt\nHello world.");
    }

    #[tokio::test]
    async fn extract_with_embedded_files() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-test-extract-files-")
            .tempdir()
            .unwrap();
        let tmp = tmp_dir.path();

        let def = BundledSkillDefinition {
            name: "files-skill".into(),
            description: "Test".into(),
            prompt_body: "# Main".into(),
            files: vec![("examples/demo.md".into(), "Demo content".into())],
            ..Default::default()
        };

        do_extract(tmp, &def).await.unwrap();

        let main = tokio::fs::read_to_string(tmp.join("SKILL.md"))
            .await
            .unwrap();
        assert_eq!(main, "# Main");

        let demo = tokio::fs::read_to_string(tmp.join("examples/demo.md"))
            .await
            .unwrap();
        assert_eq!(demo, "Demo content");
    }

    #[tokio::test]
    async fn safe_write_file_rejects_existing() {
        let tmp_dir = tempfile::Builder::new()
            .prefix("rebon-test-safe-write-")
            .tempdir()
            .unwrap();

        let file = tmp_dir.path().join("existing.md");
        tokio::fs::write(&file, "original").await.unwrap();

        // Second write should fail (O_EXCL).
        let err = safe_write_file(&file, "overwrite").await;
        assert!(err.is_err());

        // Original content preserved.
        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(content, "original");
    }
}
