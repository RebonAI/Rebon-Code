use crate::edit::optional_bool;
use crate::{Tool, ToolContext};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use ignore::types::{Types, TypesBuilder};
use ignore::{DirEntry, WalkBuilder};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use regex::RegexBuilder;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const GREP_TOOL_NAME: &str = "Grep";
const INVALID_INPUT_CODE: i64 = 400;
const PATH_NOT_FOUND_CODE: i64 = 1;
const DEFAULT_HEAD_LIMIT: usize = 250;
const VCS_DIRECTORIES_TO_EXCLUDE: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

// Same limit as ripgrep's `--max-columns 500`.
// Lines longer than this still count as matches (file still reported in
// `files_with_matches`/`count` modes) but their content is replaced with a
// placeholder so minified bundles / base64 blobs don't blast the model's
// context window.
const MAX_LINE_LEN: usize = 500;

// Hard per-file size cap fed into `ignore::WalkBuilder::max_filesize`. Files
// over this are skipped entirely — they are almost always generated
// artifacts (source maps, bundled JS, lock snapshots). This stands in for
// rg's default binary/large-file heuristics, which the `ignore` walker does
// not provide.
const MAX_FILE_BYTES: u64 = 2_000_000;

#[derive(Debug, Clone, Default)]
pub struct GrepTool;

#[derive(Debug, Clone)]
struct GrepInput {
    pattern: String,
    path: Option<PathBuf>,
    glob: Option<String>,
    output_mode: OutputMode,
    show_line_numbers: bool,
    case_insensitive: bool,
    head_limit: Option<usize>,
    offset: usize,
    /// Match across line boundaries against the whole file instead of one
    /// line at a time. `^`/`$` still anchor per line, which is what makes
    /// `(?m)`-shaped patterns behave the way ripgrep's `--multiline` does.
    multiline: bool,
    /// A ripgrep file type (`rust`, `py`, `js`, …). Resolved against
    /// ripgrep's own default type definitions.
    file_type: Option<String>,
    /// Lines of trailing / leading context around each match, `content`
    /// mode only.
    after_context: usize,
    before_context: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    /// The line the pattern matched on.
    Match,
    /// A neighbour pulled in by `-A` / `-B` / `-C`.
    Context,
}

#[derive(Debug, Clone)]
struct MatchLine {
    file: PathBuf,
    line_number: usize,
    line: String,
    kind: LineKind,
}

#[derive(Debug, Clone)]
struct FileMatch {
    file: PathBuf,
    count: usize,
}

#[async_trait]
impl Tool for GrepTool {
    fn id(&self) -> ToolId {
        ToolId::new(GREP_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["GrepTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Search
    }

    fn description(&self) -> &str {
        "A powerful search tool built on ripgrep\n\
         \n\
         Usage:\n\
         - ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` as a Bash command. \
         The Grep tool has been optimized for correct permissions and access.\n\
         - Supports full regex syntax (e.g., \"log.*Error\", \"function\\\\s+\\\\w+\")\n\
         - Filter files with the glob parameter (e.g., \"*.js\", \"**/*.tsx\") or the \
         type parameter (e.g., \"js\", \"py\", \"rust\")\n\
         - Output modes: \"content\" shows matching lines, \"files_with_matches\" shows only file paths (default), \
         \"count\" shows match counts\n\
         - Context in \"content\" mode: \"-A\" / \"-B\" / \"-C\" (or \"context\") add trailing / \
         leading / surrounding lines. A context row is printed with `-` where a match uses `:`\n\
         - Use Agent tool for open-ended searches requiring multiple rounds\n\
         - Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping \
         (use `interface\\\\{\\\\}` to find `interface{}` in Go code)\n\
         - Multiline matching: By default patterns match within single lines only. \
         For cross-line patterns like `struct \\\\{[\\\\s\\\\S]*?field`, use `multiline: true`"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "glob": { "type": "string" },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"]
                },
                "-n": { "type": "boolean" },
                "-i": { "type": "boolean" },
                "head_limit": { "type": "integer", "minimum": 0 },
                "offset": { "type": "integer", "minimum": 0 },
                "-A": { "type": "integer", "minimum": 0 },
                "-B": { "type": "integer", "minimum": 0 },
                "-C": { "type": "integer", "minimum": 0 },
                "context": { "type": "integer", "minimum": 0 },
                "type": { "type": "string" },
                "multiline": { "type": "boolean" }
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
        let regex = RegexBuilder::new(&parsed.pattern)
            .case_insensitive(parsed.case_insensitive)
            // Only meaningful in multiline mode, where the haystack is the
            // whole file: it keeps `^`/`$` anchored per line instead of to
            // the file's two ends.
            .multi_line(parsed.multiline)
            .build()
            .map_err(|err| ToolError::InvalidInput {
                tool: self.id(),
                reason: format!("Invalid regex pattern `{}`: {err}", parsed.pattern),
                error_code: Some(INVALID_INPUT_CODE),
            })?;

        let cwd = resolve_cwd(self.id(), context)?;
        let search_root = parsed
            .path
            .as_ref()
            .map(|path| crate::path_scope::resolve_context_path(path, &cwd, context))
            .unwrap_or_else(|| cwd.clone());
        crate::path_scope::enforce_read_path_policy(self.id(), context, &search_root, "path")?;
        let glob_matcher = build_glob_matcher(parsed.glob.as_deref())?;
        let type_matcher = build_type_matcher(parsed.file_type.as_deref())?;
        let files = collect_files(&search_root, glob_matcher.as_ref(), type_matcher.as_ref())?;
        let mut matched_lines = Vec::new();
        let mut file_counts = Vec::new();

        for file in files {
            let text = match fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let lines: Vec<&str> = text.lines().collect();

            // The 1-based line numbers the pattern landed on. A multiline
            // match claims every line it spans, so context expansion and the
            // line-oriented output below work the same either way.
            let mut hit_lines: BTreeSet<usize> = BTreeSet::new();
            let count = if parsed.multiline {
                let mut matches = 0;
                for found in regex.find_iter(&text) {
                    matches += 1;
                    let first = line_of_offset(&text, found.start());
                    let last = line_of_offset(&text, found.end().saturating_sub(1).max(found.start()));
                    for line_number in first..=last {
                        hit_lines.insert(line_number);
                    }
                }
                matches
            } else {
                for (idx, line) in lines.iter().enumerate() {
                    if regex.is_match(line) {
                        hit_lines.insert(idx + 1);
                    }
                }
                hit_lines.len()
            };

            if count == 0 {
                continue;
            }

            // Context shapes `content` output only: `count` stays a count of
            // matches, and `files_with_matches` never reads these rows.
            if parsed.output_mode == OutputMode::Content {
                let mut wanted: BTreeMap<usize, LineKind> = BTreeMap::new();
                for &line_number in &hit_lines {
                    let first = line_number.saturating_sub(parsed.before_context).max(1);
                    let last = line_number
                        .saturating_add(parsed.after_context)
                        .min(lines.len());
                    for neighbour in first..=last {
                        wanted.entry(neighbour).or_insert(LineKind::Context);
                    }
                    wanted.insert(line_number, LineKind::Match);
                }
                for (line_number, kind) in wanted {
                    matched_lines.push(MatchLine {
                        file: file.clone(),
                        line_number,
                        line: clamp_line(lines[line_number - 1]),
                        kind,
                    });
                }
            }

            file_counts.push(FileMatch { file, count });
        }

        file_counts.sort_by(|left, right| left.file.cmp(&right.file));
        matched_lines.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.line_number.cmp(&right.line_number))
        });

        let filenames: Vec<String> = file_counts
            .iter()
            .map(|item| display_path(&item.file, &cwd, &search_root))
            .collect();
        let num_files = filenames.len();
        let total_matches: usize = file_counts.iter().map(|item| item.count).sum();

        match parsed.output_mode {
            OutputMode::FilesWithMatches => {
                let (items, applied_limit) =
                    apply_window(filenames.clone(), parsed.head_limit, parsed.offset);
                Ok(json!({
                    "mode": "files_with_matches",
                    "numFiles": items.len(),
                    "filenames": items,
                    "appliedLimit": applied_limit,
                    "appliedOffset": (parsed.offset != 0).then_some(parsed.offset),
                }))
            }
            OutputMode::Count => {
                let rows: Vec<String> = file_counts
                    .iter()
                    .map(|item| {
                        format!(
                            "{}:{}",
                            display_path(&item.file, &cwd, &search_root),
                            item.count
                        )
                    })
                    .collect();
                let (items, applied_limit) = apply_window(rows, parsed.head_limit, parsed.offset);
                Ok(json!({
                    "mode": "count",
                    "numFiles": num_files,
                    "filenames": filenames,
                    "content": if items.is_empty() { None::<String> } else { Some(items.join("\n")) },
                    "numMatches": total_matches,
                    "appliedLimit": applied_limit,
                    "appliedOffset": (parsed.offset != 0).then_some(parsed.offset),
                }))
            }
            OutputMode::Content => {
                let rows: Vec<String> = matched_lines
                    .iter()
                    .map(|item| {
                        format_content_line(item, &cwd, &search_root, parsed.show_line_numbers)
                    })
                    .collect();
                let (items, applied_limit) = apply_window(rows, parsed.head_limit, parsed.offset);
                Ok(json!({
                    "mode": "content",
                    "numFiles": num_files,
                    "filenames": filenames,
                    "content": if items.is_empty() { None::<String> } else { Some(items.join("\n")) },
                    "numLines": items.len(),
                    "appliedLimit": applied_limit,
                    "appliedOffset": (parsed.offset != 0).then_some(parsed.offset),
                }))
            }
        }
    }
}

/// The verdict on a parsed request's search root, for `validate_input`.
///
/// Deliberately not shared with `call`: validation asks the read policy about
/// the path **as written** and reports whether it exists, while `call`
/// resolves the root against the cwd first. Merging the two would move the
/// policy question onto a different path.
fn search_root_verdict(
    tool: ToolId,
    parsed: &GrepInput,
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
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ValidationOutcome::invalid(
            format!("Path does not exist: {}", path.display()),
            PATH_NOT_FOUND_CODE,
        )),
        Err(err) => Err(ToolError::Execution {
            tool,
            source: err.into(),
        }),
    }
}

fn parse_input(input: &Value) -> ToolResult<GrepInput> {
    let tool = ToolId::new(GREP_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Grep input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let pattern = object
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "Grep input requires a non-empty `pattern` string".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_owned();

    let path = optional_string(object.get("path"), "path", &tool)?.map(PathBuf::from);
    let glob = optional_string(object.get("glob"), "glob", &tool)?;
    let output_mode =
        match optional_string(object.get("output_mode"), "output_mode", &tool)?.as_deref() {
            Some("content") => OutputMode::Content,
            Some("count") => OutputMode::Count,
            Some("files_with_matches") | None => OutputMode::FilesWithMatches,
            Some(other) => {
                return Err(ToolError::InvalidInput {
                    tool,
                    reason: format!("Unsupported output_mode `{other}`"),
                    error_code: Some(INVALID_INPUT_CODE),
                })
            }
        };

    // `-C` and its long spelling `context` set both sides; an explicit `-A`
    // or `-B` wins over them, which is how ripgrep resolves the same trio.
    let both = match optional_usize(object.get("-C"), "-C", &tool)? {
        Some(value) => Some(value),
        None => optional_usize(object.get("context"), "context", &tool)?,
    };
    let after_context = optional_usize(object.get("-A"), "-A", &tool)?
        .or(both)
        .unwrap_or(0);
    let before_context = optional_usize(object.get("-B"), "-B", &tool)?
        .or(both)
        .unwrap_or(0);

    Ok(GrepInput {
        pattern,
        path,
        glob,
        output_mode,
        show_line_numbers: optional_bool(object.get("-n"), "-n", &tool)?.unwrap_or(true),
        case_insensitive: optional_bool(object.get("-i"), "-i", &tool)?.unwrap_or(false),
        head_limit: optional_usize(object.get("head_limit"), "head_limit", &tool)?,
        offset: optional_usize(object.get("offset"), "offset", &tool)?.unwrap_or(0),
        multiline: optional_bool(object.get("multiline"), "multiline", &tool)?.unwrap_or(false),
        file_type: optional_string(object.get("type"), "type", &tool)?,
        after_context,
        before_context,
    })
}

/// Resolve a ripgrep file-type name against ripgrep's own default
/// definitions, so `type: "rust"` selects exactly what `rg --type rust`
/// would. An unknown name is a caller error, not an empty result set —
/// silently searching nothing is the failure mode this avoids.
fn build_type_matcher(name: Option<&str>) -> ToolResult<Option<Types>> {
    let Some(name) = name else {
        return Ok(None);
    };
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    builder.select(name);
    builder
        .build()
        .map(Some)
        .map_err(|err| ToolError::InvalidInput {
            tool: ToolId::new(GREP_TOOL_NAME),
            reason: format!("Unknown type `{name}`: {err}"),
            error_code: Some(INVALID_INPUT_CODE),
        })
}

fn optional_string(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<String>> {
    match value {
        Some(Value::String(raw)) if !raw.trim().is_empty() => Ok(Some(raw.clone())),
        Some(Value::String(_)) | Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be a string when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

fn optional_usize(value: Option<&Value>, field: &str, tool: &ToolId) -> ToolResult<Option<usize>> {
    match value {
        Some(Value::Number(raw)) => {
            raw.as_u64()
                .map(|v| Some(v as usize))
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: format!("`{field}` must be an integer >= 0"),
                    error_code: Some(INVALID_INPUT_CODE),
                })
        }
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be an integer when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

fn build_glob_matcher(glob: Option<&str>) -> ToolResult<Option<GlobMatcher>> {
    match glob {
        Some(pattern) => Glob::new(pattern)
            .map(|glob| Some(glob.compile_matcher()))
            .map_err(|err| ToolError::InvalidInput {
                tool: ToolId::new(GREP_TOOL_NAME),
                reason: format!("Invalid glob pattern `{pattern}`: {err}"),
                error_code: Some(INVALID_INPUT_CODE),
            }),
        None => Ok(None),
    }
}

/// Resolve the search-root cwd. Prefers [`ToolContext::cwd`] so
/// sub-agents with a worktree cwd override stay inside that tree;
/// falls back to the process cwd otherwise. Matches the helper of
/// the same name in [`crate::glob`].
fn resolve_cwd(tool: ToolId, context: &ToolContext) -> ToolResult<PathBuf> {
    if let Some(cwd) = context.cwd() {
        return Ok(PathBuf::from(cwd));
    }
    std::env::current_dir().map_err(|err| ToolError::Execution {
        tool,
        source: err.into(),
    })
}

fn collect_files(
    root: &Path,
    glob: Option<&GlobMatcher>,
    types: Option<&Types>,
) -> ToolResult<Vec<PathBuf>> {
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }

    // `ignore::WalkBuilder` is what ripgrep drives internally, so this gives
    // us `.gitignore`/`.ignore`/global-excludes respect for free — the single
    // biggest reason a naive `walkdir` over a monorepo falls over on
    // `node_modules`/`target`/`dist`. `hidden(false)` keeps the explicit
    // `--hidden` behaviour of runtime behavior (search dotfiles), while the
    // `filter_entry` VCS guard is belt-and-braces for repos that keep `.svn`
    // etc. unignored.
    let mut files = Vec::new();
    let mut builder = WalkBuilder::new(root);
    if let Some(types) = types {
        builder.types(types.clone());
    }
    let walker = builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .follow_links(false)
        .max_filesize(Some(MAX_FILE_BYTES))
        .filter_entry(should_visit)
        .build();

    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().map(|ft| ft.is_file()) != Some(true) {
            continue;
        }

        let candidate = entry.into_path();
        let normalized = candidate
            .strip_prefix(root)
            .unwrap_or(&candidate)
            .to_string_lossy()
            .replace('\\', "/");

        if let Some(matcher) = glob {
            if !matcher.is_match(&normalized) {
                continue;
            }
        }

        // A search reads what it walks over, so a session credential inside the
        // tree would come back as matched lines — the same content `Read`
        // refuses. Left out of the walk rather than failing the search, since
        // the caller asked about a directory, not about this file.
        if crate::path_scope::is_session_credential_path(&candidate) {
            continue;
        }

        files.push(candidate);
    }
    Ok(files)
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !VCS_DIRECTORIES_TO_EXCLUDE
        .iter()
        .any(|item| item == &name.as_ref())
}

fn display_path(path: &Path, cwd: &Path, root: &Path) -> String {
    path.strip_prefix(cwd)
        .or_else(|_| path.strip_prefix(root))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn format_content_line(
    item: &MatchLine,
    cwd: &Path,
    root: &Path,
    show_line_numbers: bool,
) -> String {
    let file = display_path(&item.file, cwd, root);
    // ripgrep's separator convention: `:` introduces a match, `-` a context
    // line. Without it a caller asking for context cannot tell which line
    // actually matched.
    let sep = match item.kind {
        LineKind::Match => ':',
        LineKind::Context => '-',
    };
    if show_line_numbers {
        format!("{file}{sep}{}{sep}{}", item.line_number, item.line)
    } else {
        format!("{file}{sep}{}", item.line)
    }
}

/// Matches rg's `--max-columns 500`: a long line still counts as a match but
/// its content is replaced, so one minified bundle cannot devour the
/// caller's context window.
fn clamp_line(line: &str) -> String {
    if line.len() > MAX_LINE_LEN {
        format!("[line omitted: {} bytes with match]", line.len())
    } else {
        line.to_owned()
    }
}

/// 1-based line number containing `offset`, for multiline matches whose
/// position only comes back as a byte offset into the whole file.
fn line_of_offset(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn apply_window<T>(items: Vec<T>, limit: Option<usize>, offset: usize) -> (Vec<T>, Option<usize>) {
    let effective_limit = limit.unwrap_or(DEFAULT_HEAD_LIMIT);
    if limit == Some(0) {
        return (items.into_iter().skip(offset).collect(), None);
    }
    let total_after_offset = items.len().saturating_sub(offset);
    let sliced = items
        .into_iter()
        .skip(offset)
        .take(effective_limit)
        .collect::<Vec<_>>();
    let truncated = total_after_offset > effective_limit;
    (sliced, truncated.then_some(effective_limit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TempDir {
        inner: tempfile::TempDir,
    }

    impl TempDir {
        fn new() -> Self {
            Self {
                inner: tempfile::Builder::new()
                    .prefix("rebon-grep-tool-test-")
                    .tempdir()
                    .unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }
    }

    fn tool() -> GrepTool {
        GrepTool
    }

    #[tokio::test]
    async fn validate_input_rejects_missing_path() {
        let input = json!({
            "pattern": "needle",
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
                format!("Path does not exist: {}", missing_path.display()),
                1
            )
        );
    }

    #[tokio::test]
    async fn call_returns_matching_files_mode() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), "needle here\n").unwrap();
        fs::write(dir.path().join("b.txt"), "nothing\n").unwrap();
        fs::write(dir.path().join("c.txt"), "needle twice\nneedle again\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["mode"], json!("files_with_matches"));
        assert_eq!(out["numFiles"], json!(2));
        assert_eq!(out["filenames"], json!(["a.txt", "c.txt"]));
    }

    #[tokio::test]
    async fn call_returns_content_mode_with_line_numbers() {
        let dir = TempDir::new();
        fs::write(dir.path().join("demo.rs"), "fn needle() {}\nlet x = 1;\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "content",
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["mode"], json!("content"));
        assert_eq!(out["numLines"], json!(1));
        assert_eq!(out["content"], json!("demo.rs:1:fn needle() {}"));
    }

    #[tokio::test]
    async fn context_surrounds_the_match_and_marks_itself_with_a_dash() {
        let dir = TempDir::new();
        fs::write(dir.path().join("demo.rs"), "one\ntwo\nneedle\nfour\nfive\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "content",
                    "-C": 1,
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        // `:` on the match, `-` on its neighbours — the distinction a caller
        // needs to tell which line the pattern actually landed on.
        assert_eq!(
            out["content"],
            json!("demo.rs-2-two\ndemo.rs:3:needle\ndemo.rs-4-four")
        );
    }

    #[tokio::test]
    async fn after_and_before_context_are_independent() {
        let dir = TempDir::new();
        fs::write(dir.path().join("demo.rs"), "one\ntwo\nneedle\nfour\nfive\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "content",
                    "-A": 2,
                    "-B": 0,
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            out["content"],
            json!("demo.rs:3:needle\ndemo.rs-4-four\ndemo.rs-5-five")
        );
    }

    #[tokio::test]
    async fn an_explicit_side_wins_over_the_context_alias() {
        let dir = TempDir::new();
        fs::write(dir.path().join("demo.rs"), "one\ntwo\nneedle\nfour\nfive\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "content",
                    "context": 1,
                    "-A": 0,
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["content"], json!("demo.rs-2-two\ndemo.rs:3:needle"));
    }

    #[tokio::test]
    async fn multiline_matches_across_a_line_boundary() {
        let dir = TempDir::new();
        fs::write(
            dir.path().join("demo.rs"),
            "struct Thing {\n    field: u8,\n}\n",
        )
        .unwrap();
        let input = json!({
            "pattern": r"struct \{?[\s\S]*?field",
            "path": dir.path().to_string_lossy(),
            "output_mode": "content",
            "multiline": true,
        });

        let out = tool().call(input, &ToolContext::new()).await.unwrap();

        // Every line the match spans comes back, line-oriented.
        assert_eq!(
            out["content"],
            json!("demo.rs:1:struct Thing {\ndemo.rs:2:    field: u8,")
        );
    }

    #[tokio::test]
    async fn without_multiline_the_same_pattern_finds_nothing() {
        let dir = TempDir::new();
        fs::write(
            dir.path().join("demo.rs"),
            "struct Thing {\n    field: u8,\n}\n",
        )
        .unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": r"struct \{?[\s\S]*?field",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "content",
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["numFiles"], json!(0));
    }

    #[tokio::test]
    async fn type_filters_to_ripgreps_own_definition() {
        let dir = TempDir::new();
        fs::write(dir.path().join("keep.rs"), "needle\n").unwrap();
        fs::write(dir.path().join("skip.txt"), "needle\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "type": "rust",
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["filenames"], json!(["keep.rs"]));
    }

    #[tokio::test]
    async fn an_unknown_type_is_refused_rather_than_silently_empty() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.rs"), "needle\n").unwrap();

        let err = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "type": "not-a-real-type",
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert!(reason.contains("not-a-real-type"), "{reason}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_does_not_inflate_the_count() {
        let dir = TempDir::new();
        fs::write(dir.path().join("demo.txt"), "one\nneedle\nthree\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "count",
                    "-C": 1,
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["numMatches"], json!(1));
    }

    #[tokio::test]
    async fn call_returns_count_mode() {
        let dir = TempDir::new();
        fs::write(dir.path().join("demo.txt"), "needle\nneedle\nother\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "count",
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["mode"], json!("count"));
        assert_eq!(out["numFiles"], json!(1));
        assert_eq!(out["numMatches"], json!(2));
        assert_eq!(out["content"], json!("demo.txt:2"));
    }

    #[tokio::test]
    async fn call_applies_glob_and_offset_window() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
        fs::write(dir.path().join("c.rs"), "needle\n").unwrap();
        fs::write(dir.path().join("d.rs"), "needle\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "glob": "*.rs",
                    "offset": 1,
                    "head_limit": 1,
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["filenames"], json!(["c.rs"]));
        assert_eq!(out["appliedLimit"], json!(1));
        assert_eq!(out["appliedOffset"], json!(1));
    }

    #[tokio::test]
    async fn sub_agent_grep_rejects_search_root_outside_scope() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        fs::write(outside.path().join("demo.txt"), "needle\n").unwrap();

        let result = tool()
            .call(
                json!({
                    "pattern": "needle",
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
    fn grep_tool_metadata_has_expected_shape() {
        let tool = tool();
        assert_eq!(tool.id().as_str(), "Grep");
        assert!(tool.is_concurrency_safe(&json!({})));
        assert!(tool.is_read_only(&json!({})));
    }

    #[tokio::test]
    async fn call_respects_gitignore() {
        // Build a mini git repo so the `ignore` crate's `require_git=true`
        // default activates and `.gitignore` is honoured the same way
        // ripgrep would in a real checkout.
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("ignored").join("bundle.js"), "needle\n").unwrap();
        fs::write(dir.path().join("tracked.rs"), "needle\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let names = out["filenames"].as_array().unwrap();
        let names: Vec<_> = names.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(
            names.contains(&"tracked.rs"),
            "tracked file must match: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("ignored")),
            "gitignored dir must be filtered: {names:?}"
        );
    }

    #[tokio::test]
    async fn call_excludes_vcs_metadata_directories() {
        let dir = TempDir::new();
        // Intentionally no `.gitignore` — the hardcoded VCS filter in
        // `should_visit` must still keep `.git/` contents out of results.
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git").join("HEAD"), "ref: needle\n").unwrap();
        fs::write(dir.path().join("real.txt"), "needle\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["filenames"], json!(["real.txt"]));
    }

    #[tokio::test]
    async fn call_includes_hidden_files_like_src_hidden_flag() {
        // Src passes `--hidden` to rg (runtime behavior); match that so
        // searches over dotfiles (e.g. `.env.example`) still land matches.
        let dir = TempDir::new();
        fs::write(dir.path().join(".envrc"), "needle=1\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["filenames"], json!([".envrc"]));
    }

    #[tokio::test]
    async fn call_caps_long_line_content() {
        let dir = TempDir::new();
        let blob = "x".repeat(MAX_LINE_LEN + 50);
        let line = format!("needle-{blob}");
        fs::write(dir.path().join("min.js"), format!("{line}\n")).unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                    "output_mode": "content",
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["mode"], json!("content"));
        assert_eq!(out["numLines"], json!(1));
        let content = out["content"].as_str().unwrap();
        assert!(
            content.starts_with("min.js:1:[line omitted:"),
            "long line must be placeholdered, got: {content}"
        );
        assert!(
            !content.contains(&"x".repeat(MAX_LINE_LEN)),
            "raw blob must not leak into output"
        );
    }

    #[tokio::test]
    async fn call_skips_files_larger_than_cap() {
        let dir = TempDir::new();
        // One file over the cap (must be skipped entirely) and one under
        // (must still match). Use an exact multiple of the cap + slack so
        // platform block sizes don't accidentally round us under.
        let mut huge = vec![b'.'; (MAX_FILE_BYTES as usize) + 64];
        huge.extend_from_slice(b"\nneedle\n");
        fs::write(dir.path().join("huge.log"), &huge).unwrap();
        fs::write(dir.path().join("ok.txt"), "needle\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_string_lossy(),
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            out["filenames"],
            json!(["ok.txt"]),
            "only the small file should be searched"
        );
    }
}
