use crate::search_walk::{self, WalkLimits};
use crate::{Tool, ToolContext};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

const GLOB_TOOL_NAME: &str = "Glob";
const DESCRIPTION: &str = "- Fast file pattern matching tool
- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"
- Returns matching file paths sorted by modification time
- `*` and `**/*` match every file recursively; set `path` to the narrowest directory you need (default: the working directory). Do not run broad patterns on parent folders, drive roots, or build output (target/, node_modules/, a CARGO_TARGET_DIR); those are skipped below `path`, and the walk stops after 100k entries or 10s
- Use this tool when you need to find files by name patterns
- Prefer investigating directly; consider Explore for extensive independent research with clear parallel or context benefits";
const DEFAULT_LIMIT: usize = 100;
/// Matches collected (each costing a metadata read) before the walk stops.
/// Generous next to the 100 returned, so the newest-first order is real for
/// any search specific enough to be useful.
const MAX_COLLECTED_MATCHES: usize = DEFAULT_LIMIT * 10;
const INVALID_INPUT_CODE: i64 = 400;
const MISSING_DIRECTORY_CODE: i64 = 1;
const NOT_DIRECTORY_CODE: i64 = 2;

#[derive(Debug, Clone, Default)]
pub struct GlobTool;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobInput {
    pattern: String,
    path: Option<PathBuf>,
}

#[derive(Debug)]
struct GlobMatch {
    path: PathBuf,
    modified: Option<SystemTime>,
}

#[async_trait]
impl Tool for GlobTool {
    fn id(&self) -> ToolId {
        ToolId::new(GLOB_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["GlobTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Search
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against"
                },
                "path": {
                    "type": "string",
                    "description": "Optional directory to search in. Defaults to the current working directory."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match parse_input(input) {
            Ok(parsed) => search_root_verdict(self.id(), &parsed, _context),
            refused => validation_outcome_from(refused),
        }
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        run_glob(input, context, WalkLimits::default(), MAX_COLLECTED_MATCHES).await
    }
}

async fn run_glob(
    input: Value,
    context: &ToolContext,
    limits: WalkLimits,
    max_matches: usize,
) -> ToolResult<Value> {
    let tool = ToolId::new(GLOB_TOOL_NAME);
    let parsed = parse_input(&input)?;
    let cwd = resolve_cwd(tool.clone(), context)?;
    let search_root = parsed
        .path
        .as_ref()
        .map(|path| crate::path_scope::resolve_context_path(path, &cwd, context))
        .unwrap_or_else(|| cwd.clone());
    crate::path_scope::enforce_read_path_policy(tool.clone(), context, &search_root, "path")?;
    let matcher = build_matcher(&parsed.pattern)?;

    // Off the async worker, so a big tree neither pins a tokio thread nor
    // outlives an interrupt: dropping this future stops the walk.
    search_walk::run_cancellable(tool, move |stop| {
        Ok(glob_blocking(
            &search_root,
            &cwd,
            &matcher,
            &limits,
            max_matches,
            &stop,
        ))
    })
    .await
}

fn glob_blocking(
    search_root: &Path,
    cwd: &Path,
    matcher: &GlobMatcher,
    limits: &WalkLimits,
    max_matches: usize,
    stop: &Arc<AtomicBool>,
) -> Value {
    let start = Instant::now();
    let mut matches = Vec::new();

    // Traversal and gitignore semantics come from the `ignore` crate (see
    // `search_walk`), so `**/*.ts` doesn't enumerate a 200k-file
    // `node_modules`; `hidden(false)` keeps dotfile matches reachable.
    let summary = search_walk::walk(
        search_root,
        limits,
        stop,
        |_| {},
        |entry| {
            if entry.file_type().map(|ft| ft.is_file()) != Some(true) {
                return ControlFlow::Continue(());
            }
            let candidate = entry.into_path();
            if !matches_pattern(&candidate, search_root, matcher) {
                return ControlFlow::Continue(());
            }
            let modified = fs::metadata(&candidate)
                .ok()
                .and_then(|meta| meta.modified().ok());
            matches.push(GlobMatch {
                path: candidate,
                modified,
            });
            // One past the cap, so stopping here means more matches exist.
            if matches.len() > max_matches {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );

    let incomplete = match summary.stopped {
        Some(search_walk::WalkStop::Visitor) => {
            matches.truncate(max_matches);
            Some(format!(
                "Stopped after {max_matches} matches under {}; the newest of those are shown, \
                 not of every match. Use a more specific pattern or narrow `path`.",
                search_root.display()
            ))
        }
        _ => summary.incomplete_note(search_root, limits),
    };
    let truncated = matches.len() > DEFAULT_LIMIT || incomplete.is_some();

    matches.sort_by(compare_matches);

    let filenames: Vec<String> = matches
        .into_iter()
        .take(DEFAULT_LIMIT)
        .map(|m| display_path(&m.path, cwd, search_root))
        .collect();

    let duration_ms = start.elapsed().as_millis() as u64;

    let mut out = json!({
        "durationMs": duration_ms,
        "numFiles": filenames.len(),
        "filenames": filenames,
        "truncated": truncated,
    });
    if let Some(note) = incomplete {
        out["searchIncomplete"] = json!(note);
    }
    out
}

/// The verdict on a parsed request's search root, for `validate_input`.
///
/// Deliberately not shared with `call`: validation asks the read policy about
/// the path **as written**, then reports whether that directory exists, while
/// `call` resolves the root first (falling back to the cwd) and asks the
/// policy about the resolved root. Merging the two would move the policy
/// question onto a different path.
fn search_root_verdict(
    tool: ToolId,
    parsed: &GlobInput,
    context: &ToolContext,
) -> ToolResult<ValidationOutcome> {
    let Some(path) = &parsed.path else {
        return Ok(ValidationOutcome::valid());
    };
    if let Err(ToolError::InvalidInput {
        reason, error_code, ..
    }) = crate::path_scope::enforce_read_path_policy(tool.clone(), context, path, "path")
    {
        return Ok(ValidationOutcome::invalid(
            reason,
            error_code.unwrap_or(INVALID_INPUT_CODE),
        ));
    }
    let cwd = resolve_cwd(tool.clone(), context)?;
    let path = crate::path_scope::resolve_context_path(path, &cwd, context);
    match fs::metadata(&path) {
        Ok(metadata) if !metadata.is_dir() => Ok(ValidationOutcome::invalid(
            format!("Path is not a directory: {}", path.display()),
            NOT_DIRECTORY_CODE,
        )),
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ValidationOutcome::invalid(
            format!("Directory does not exist: {}", path.display()),
            MISSING_DIRECTORY_CODE,
        )),
        Err(err) => Err(ToolError::Execution {
            tool,
            source: err.into(),
        }),
    }
}

fn parse_input(input: &Value) -> ToolResult<GlobInput> {
    let tool = ToolId::new(GLOB_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Glob input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let pattern = object
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "Glob input requires a non-empty `pattern` string".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_owned();

    let path = match object.get("path") {
        Some(Value::String(path)) if !path.trim().is_empty() => Some(PathBuf::from(path)),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool,
                reason: "`path` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    Ok(GlobInput { pattern, path })
}

fn build_matcher(pattern: &str) -> ToolResult<GlobMatcher> {
    let tool = ToolId::new(GLOB_TOOL_NAME);
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher())
        .map_err(|err| ToolError::InvalidInput {
            tool,
            reason: format!("Invalid glob pattern `{pattern}`: {err}"),
            error_code: Some(INVALID_INPUT_CODE),
        })
}

fn matches_pattern(path: &Path, root: &Path, matcher: &GlobMatcher) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let normalized = rel.to_string_lossy().replace('\\', "/");
    matcher.is_match(normalized)
}

/// Resolve the search-root cwd for this tool invocation. Prefer
/// the caller-supplied [`ToolContext::cwd`] so worktree-isolated
/// sub-agents stay inside
/// their override; fall back to the process cwd otherwise.
fn resolve_cwd(tool: ToolId, context: &ToolContext) -> ToolResult<PathBuf> {
    if let Some(cwd) = context.cwd() {
        return Ok(PathBuf::from(cwd));
    }
    std::env::current_dir().map_err(|err| ToolError::Execution {
        tool,
        source: err.into(),
    })
}

fn compare_matches(left: &GlobMatch, right: &GlobMatch) -> Ordering {
    match (left.modified, right.modified) {
        (Some(l), Some(r)) => r.cmp(&l).then_with(|| left.path.cmp(&right.path)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.path.cmp(&right.path),
    }
}

fn display_path(path: &Path, cwd: &Path, root: &Path) -> String {
    path.strip_prefix(cwd)
        .or_else(|_| path.strip_prefix(root))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use std::time::Duration;

    #[test]
    fn description_does_not_force_delegation_for_multiple_searches() {
        assert!(!GlobTool
            .description()
            .contains("use the Agent tool instead"));
    }

    struct TempDir {
        inner: tempfile::TempDir,
    }

    impl TempDir {
        fn new() -> Self {
            Self {
                inner: tempfile::Builder::new()
                    .prefix("rebon-glob-tool-test-")
                    .tempdir()
                    .unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }
    }

    fn tool() -> GlobTool {
        GlobTool
    }

    #[tokio::test]
    async fn validate_input_rejects_missing_directory() {
        let input = json!({
            "pattern": "**/*.rs",
            "path": "does-not-exist"
        });

        let result = tool()
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap();

        let missing_path = std::env::current_dir().unwrap().join("does-not-exist");
        assert_eq!(
            result,
            ValidationOutcome::invalid(
                format!("Directory does not exist: {}", missing_path.display()),
                1
            )
        );
    }

    #[tokio::test]
    async fn validate_input_rejects_non_directory_path() {
        let dir = TempDir::new();
        let file = dir.path().join("single.txt");
        fs::write(&file, "hello").unwrap();
        let input = json!({
            "pattern": "*.txt",
            "path": file.to_string_lossy(),
        });

        let result = tool()
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(
            result,
            ValidationOutcome::invalid(format!("Path is not a directory: {}", file.display()), 2)
        );
    }

    #[tokio::test]
    async fn call_returns_matching_files_relative_to_cwd() {
        let dir = TempDir::new();
        let nested = dir.path().join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(dir.path().join("Cargo.toml"), "workspace").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(nested.join("lib.rs"), "pub fn demo() {}").unwrap();

        let input = json!({
            "pattern": "**/*.rs",
            "path": dir.path().to_string_lossy(),
        });

        let out = tool().call(input, &ToolContext::new()).await.unwrap();

        assert_eq!(out["numFiles"], json!(1));
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["filenames"], json!(["src/lib.rs"]));
        assert!(out["durationMs"].as_u64().is_some());
    }

    #[tokio::test]
    async fn call_truncates_after_default_limit() {
        let dir = TempDir::new();
        for idx in 0..(DEFAULT_LIMIT + 1) {
            fs::write(dir.path().join(format!("file-{idx}.txt")), "x").unwrap();
        }

        let input = json!({
            "pattern": "*.txt",
            "path": dir.path().to_string_lossy(),
        });

        let out = tool().call(input, &ToolContext::new()).await.unwrap();
        let filenames = out["filenames"].as_array().unwrap();

        assert_eq!(filenames.len(), DEFAULT_LIMIT);
        assert_eq!(out["numFiles"], json!(DEFAULT_LIMIT));
        assert_eq!(out["truncated"], json!(true));
    }

    #[tokio::test]
    async fn sub_agent_glob_rejects_search_root_outside_scope() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        fs::write(outside.path().join("Cargo.toml"), "workspace").unwrap();

        let result = tool()
            .call(
                json!({
                    "pattern": "*.toml",
                    "path": outside.path().to_string_lossy(),
                }),
                &ToolContext::new()
                    .with_agent_id("agent-test")
                    .with_cwd(dir.path().to_string_lossy()),
            )
            .await
            .unwrap_err();

        assert!(result.to_string().contains("outside that scope"));
    }

    #[test]
    fn input_schema_mentions_pattern_and_path() {
        let schema = tool().input_schema();
        assert_eq!(schema["type"], json!("object"));
        assert!(schema["properties"]["pattern"].is_object());
        assert!(schema["properties"]["path"].is_object());
    }

    #[test]
    fn glob_tool_metadata_has_expected_shape() {
        let tool = tool();
        assert_eq!(tool.id().as_str(), "Glob");
        assert!(tool.is_concurrency_safe(&json!({})));
        assert!(tool.is_read_only(&json!({})));
        assert!(!tool.needs_permission(&json!({})));
    }

    #[tokio::test]
    async fn call_respects_gitignore_for_large_dirs() {
        let dir = TempDir::new();
        // Simulate a repo with `node_modules` gitignored. Without gitignore
        // respect, a `**/*.js` glob would pick up every vendored file.
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        let vendored = dir.path().join("node_modules").join("dep");
        fs::create_dir_all(&vendored).unwrap();
        fs::write(vendored.join("index.js"), "// vendored").unwrap();
        fs::write(dir.path().join("app.js"), "// tracked").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "**/*.js",
                    "path": dir.path().to_string_lossy(),
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let names: Vec<_> = out["filenames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert!(
            names.iter().any(|n| n == "app.js"),
            "tracked file must match: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "node_modules must stay filtered: {names:?}"
        );
    }

    #[tokio::test]
    async fn call_excludes_vcs_metadata_from_glob() {
        // No `.gitignore` here — verify the explicit VCS filter still fires
        // so e.g. `**/HEAD` from a user's loose `*` glob doesn't drag in
        // `.git/HEAD`.
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git").join("HEAD"), "ref: x").unwrap();
        fs::write(dir.path().join("keep.txt"), "x").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "**/*",
                    "path": dir.path().to_string_lossy(),
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let names: Vec<_> = out["filenames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert!(names.iter().any(|n| n == "keep.txt"));
        assert!(
            !names.iter().any(|n| n.starts_with(".git/")),
            "VCS metadata must stay filtered: {names:?}"
        );
    }

    fn filenames(out: &Value) -> Vec<String> {
        out["filenames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }

    async fn glob_in(dir: &Path, pattern: &str) -> Value {
        tool()
            .call(
                json!({ "pattern": pattern, "path": dir.to_string_lossy() }),
                &ToolContext::new(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn cargo_target_dir_is_skipped_outside_git_but_searchable_explicitly() {
        // No `.git` anywhere: nothing gitignores the build output, so the
        // `.rustc_info.json` marker alone has to keep it out of the walk.
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("lib.rs"), "").unwrap();
        let target = dir.path().join("shared-target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        fs::write(target.join("debug").join("build.rs"), "").unwrap();

        let out = glob_in(dir.path(), "**/*.rs").await;
        assert_eq!(filenames(&out), vec!["src/lib.rs".to_owned()]);
        assert!(out.get("searchIncomplete").is_none(), "{out}");

        let out = glob_in(&target, "**/*.rs").await;
        assert_eq!(filenames(&out), vec!["debug/build.rs".to_owned()]);
    }

    #[tokio::test]
    async fn node_modules_below_the_root_is_skipped() {
        let dir = TempDir::new();
        let vendored = dir.path().join("web").join("node_modules").join("dep");
        fs::create_dir_all(&vendored).unwrap();
        fs::write(vendored.join("index.js"), "").unwrap();
        fs::write(dir.path().join("web").join("app.js"), "").unwrap();

        let out = glob_in(dir.path(), "**/*.js").await;
        assert_eq!(filenames(&out), vec!["web/app.js".to_owned()]);

        let out = glob_in(&dir.path().join("web").join("node_modules"), "**/*.js").await;
        assert_eq!(filenames(&out), vec!["dep/index.js".to_owned()]);
    }

    #[tokio::test]
    async fn gitignore_applies_outside_a_repo() {
        // A folder of repos with its own `.gitignore` but no `.git`.
        let dir = TempDir::new();
        fs::write(dir.path().join(".gitignore"), "dist/\n").unwrap();
        fs::create_dir_all(dir.path().join("dist")).unwrap();
        fs::write(dir.path().join("dist").join("bundle.js"), "").unwrap();
        fs::write(dir.path().join("app.js"), "").unwrap();

        let out = glob_in(dir.path(), "**/*.js").await;
        assert_eq!(filenames(&out), vec!["app.js".to_owned()]);
    }

    #[tokio::test]
    async fn a_gitignore_above_the_repo_root_does_not_hide_repo_files() {
        // `outer/.gitignore` says `bin/`, but `repo` is its own checkout:
        // git would not apply the outer file there, and neither do we.
        let dir = TempDir::new();
        fs::write(dir.path().join(".gitignore"), "bin/\n").unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src").join("bin")).unwrap();
        fs::write(repo.join("src").join("bin").join("main.rs"), "").unwrap();

        let out = glob_in(&repo, "**/*.rs").await;
        assert_eq!(filenames(&out), vec!["src/bin/main.rs".to_owned()]);
    }

    #[tokio::test]
    async fn a_walk_over_the_entry_cap_says_the_search_is_incomplete() {
        let dir = TempDir::new();
        for idx in 0..20 {
            fs::write(dir.path().join(format!("f{idx}.txt")), "x").unwrap();
        }
        let limits = WalkLimits {
            max_entries: 5,
            ..WalkLimits::default()
        };

        let out = run_glob(
            json!({ "pattern": "*", "path": dir.path().to_string_lossy() }),
            &ToolContext::new(),
            limits,
            MAX_COLLECTED_MATCHES,
        )
        .await
        .unwrap();

        assert_eq!(out["truncated"], json!(true));
        let note = out["searchIncomplete"].as_str().unwrap();
        assert!(note.contains("5 entries"), "{note}");
        assert!(note.contains("Narrow `path`"), "{note}");
        // Root plus four files were visited; what was found still comes back.
        assert_eq!(out["numFiles"], json!(4));
    }

    #[tokio::test]
    async fn collection_stops_at_the_match_cap() {
        let dir = TempDir::new();
        for idx in 0..10 {
            fs::write(dir.path().join(format!("f{idx}.txt")), "x").unwrap();
        }

        let out = run_glob(
            json!({ "pattern": "*.txt", "path": dir.path().to_string_lossy() }),
            &ToolContext::new(),
            WalkLimits::default(),
            3,
        )
        .await
        .unwrap();

        assert_eq!(out["numFiles"], json!(3));
        assert_eq!(out["truncated"], json!(true));
        let note = out["searchIncomplete"].as_str().unwrap();
        assert!(note.contains("3 matches"), "{note}");
    }

    #[tokio::test]
    async fn exactly_the_match_cap_is_not_incomplete() {
        let dir = TempDir::new();
        for idx in 0..3 {
            fs::write(dir.path().join(format!("f{idx}.txt")), "x").unwrap();
        }

        let out = run_glob(
            json!({ "pattern": "*.txt", "path": dir.path().to_string_lossy() }),
            &ToolContext::new(),
            WalkLimits::default(),
            3,
        )
        .await
        .unwrap();

        assert_eq!(out["numFiles"], json!(3));
        assert_eq!(out["truncated"], json!(false));
        assert!(out.get("searchIncomplete").is_none(), "{out}");
    }

    #[tokio::test]
    async fn dropping_the_call_stops_the_blocking_walk() {
        // 300 entries at 20ms each is a 6s walk if nothing stops it.
        let dir = TempDir::new();
        for idx in 0..300 {
            fs::write(dir.path().join(format!("f{idx}.txt")), "x").unwrap();
        }
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let limits = WalkLimits {
            probe: Some(Arc::new(search_walk::WalkProbe {
                on_entry: Box::new(|| std::thread::sleep(Duration::from_millis(20))),
                on_finish: Box::new(move |summary| {
                    let _ = finished_tx.send(*summary);
                }),
            })),
            ..WalkLimits::default()
        };

        let context = ToolContext::new();
        let call = run_glob(
            json!({ "pattern": "**/*", "path": dir.path().to_string_lossy() }),
            &context,
            limits,
            MAX_COLLECTED_MATCHES,
        );
        // Timing out drops the call future, as the turn's cancel does.
        let timed_out = tokio::time::timeout(Duration::from_millis(150), call).await;
        assert!(timed_out.is_err(), "the walk must still be running");

        let summary = finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the blocking walk must stop soon after the call is dropped");
        assert_eq!(summary.stopped, Some(search_walk::WalkStop::Cancelled));
        assert!(summary.visited < 300, "{summary:?}");
    }
}
