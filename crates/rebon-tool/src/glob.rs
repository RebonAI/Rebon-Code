use crate::{Tool, ToolContext};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const GLOB_TOOL_NAME: &str = "Glob";
// Matches grep.rs — `.git`/`.svn`/etc. are noise even when the user forgot
// to gitignore them. Glob inherits the same exclusions so e.g.
// `**/HEAD` over a repo root doesn't dump internal refs.
const VCS_DIRECTORIES_TO_EXCLUDE: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];
const DESCRIPTION: &str = "- Fast file pattern matching tool that works with any codebase size
- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"
- Returns matching file paths sorted by modification time
- Use this tool when you need to find files by name patterns
- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use the Agent tool instead";
const DEFAULT_LIMIT: usize = 100;
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
        let parsed = parse_input(&input)?;
        let cwd = resolve_cwd(self.id(), context)?;
        let search_root = parsed
            .path
            .as_ref()
            .map(|path| crate::path_scope::resolve_context_path(path, &cwd, context))
            .unwrap_or_else(|| cwd.clone());
        crate::path_scope::enforce_read_path_policy(self.id(), context, &search_root, "path")?;
        let matcher = build_matcher(&parsed.pattern)?;
        let start = SystemTime::now();

        let mut matches = Vec::new();
        let mut truncated = false;

        // Same walker swap as grep.rs: defer traversal + gitignore semantics
        // to the `ignore` crate so `**/*.ts` doesn't accidentally enumerate a
        // 200k-file `node_modules`. `hidden(false)` keeps dotfile matches
        // reachable, and the VCS `filter_entry` is the belt-and-braces layer
        // for repos that don't gitignore `.svn`/`.hg`.
        let walker = WalkBuilder::new(&search_root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true)
            .follow_links(false)
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                !VCS_DIRECTORIES_TO_EXCLUDE
                    .iter()
                    .any(|item| item == &name.as_ref())
            })
            .build();

        for entry in walker.filter_map(Result::ok) {
            if entry.file_type().map(|ft| ft.is_file()) != Some(true) {
                continue;
            }

            let candidate = entry.into_path();
            if !matches_pattern(&candidate, &search_root, &matcher) {
                continue;
            }

            let modified = fs::metadata(&candidate)
                .ok()
                .and_then(|meta| meta.modified().ok());
            matches.push(GlobMatch {
                path: candidate,
                modified,
            });

            if matches.len() > DEFAULT_LIMIT {
                truncated = true;
            }
        }

        matches.sort_by(compare_matches);

        let filenames: Vec<String> = matches
            .into_iter()
            .take(DEFAULT_LIMIT)
            .map(|m| display_path(&m.path, &cwd, &search_root))
            .collect();

        let duration_ms = start.elapsed().unwrap_or(Duration::ZERO).as_millis() as u64;

        Ok(json!({
            "durationMs": duration_ms,
            "numFiles": filenames.len(),
            "filenames": filenames,
            "truncated": truncated,
        }))
    }
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
}
