//! `ReadMemory`: read durable memory without file tools.
//!
//! The router has no `Read`: everything that touches a project is a
//! worker's. It still writes project memory (`SaveMemory` with `project`),
//! and to update a memory instead of adding a near-duplicate beside it, or
//! to brief a worker with what a project already knows, it has to see what
//! is there. This is that window, and nothing more: the `MEMORY.md` index,
//! or one memory it lists, of user memory or of one project's repo memory.

use crate::memory::save::{parse_scope, validate_memory_file_name};
use crate::save_memory::{memory_cwd, project_problem};
use async_trait::async_trait;
use rebon_session::memory_paths::{memory_dir_for_scope, MemoryScope, ENTRYPOINT_NAME};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, PermissionDecision, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use serde_json::{json, Value};

pub const READ_MEMORY_TOOL_NAME: &str = "ReadMemory";
const INVALID_INPUT_CODE: i64 = 400;
/// Memories are short notes and an index of one-line hooks; past this much
/// text something is wrong, and the rest would only crowd the router's turn.
const MAX_CHARS: usize = 40_000;

#[derive(Debug, Clone, Default)]
pub struct ReadMemoryTool;

/// What to read: whose memory, and the index or one memory in it.
struct ReadRequest {
    scope: MemoryScope,
    project: Option<String>,
    /// `None` is the index itself.
    file: Option<String>,
}

#[async_trait]
impl Tool for ReadMemoryTool {
    fn id(&self) -> ToolId {
        ToolId::new(READ_MEMORY_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Read durable memory. With no `file` (omit it, or pass null), returns the MEMORY.md \
         index of user memory (scope `user`) or of a project's repo memory (scope `repo`); with \
         `file`, one memory the index lists. Naming `MEMORY.md` also means the index. `project` \
         is the absolute path of the project whose repo memory to read, and takes no part in \
         scope `user`, which is the person's memory and the same for every project. Read a \
         project's memory before saving to it, so you update a memory instead of adding one \
         beside it, and before briefing a worker there."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["user", "repo"],
                    "description": "Whose memory: the person's (`user`) or a project's (`repo`)."
                },
                "project": {
                    "type": ["string", "null"],
                    "description": "Only with scope `repo`: the absolute path of the project whose memory to read. Omit it or pass null for the project this session runs in. Never pass it with scope `user`."
                },
                "file": {
                    "type": ["string", "null"],
                    "description": "One memory the index lists, by its .md file name. Omit it, pass null, or pass an empty string for the index; `MEMORY.md` names the index too."
                }
            },
            "required": ["scope"],
            "additionalProperties": false
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_input(input))
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(PermissionDecision::allow(input.clone()))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let request = parse_input(&input)?;
        let dir = memory_dir(&request, context)?;
        let path = dir.join(request.file.as_deref().unwrap_or(ENTRYPOINT_NAME));
        let content = match std::fs::read_to_string(&path) {
            Ok(text) => clip(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if request.file.is_some() {
                    return Err(invalid(format!(
                        "no memory `{}` here; read the index for what there is",
                        path.display()
                    )));
                }
                "No memory is saved here yet.".to_string()
            }
            Err(error) => {
                return Err(ToolError::Execution {
                    tool: ToolId::new(READ_MEMORY_TOOL_NAME),
                    source: std::io::Error::new(
                        error.kind(),
                        format!("could not read {}: {error}", path.display()),
                    )
                    .into(),
                })
            }
        };
        Ok(json!({
            "path": path.to_string_lossy(),
            "content": content,
        }))
    }
}

fn clip(text: String) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut clipped: String = text.chars().take(MAX_CHARS).collect();
    clipped.push_str("\n\n[… cut off: this memory is longer than ReadMemory returns]");
    clipped
}

fn parse_input(input: &Value) -> ToolResult<ReadRequest> {
    let object = input
        .as_object()
        .ok_or_else(|| invalid("ReadMemory input must be an object".into()))?;
    let string = |field: &str| -> ToolResult<Option<String>> {
        match object.get(field) {
            Some(Value::String(raw)) => Ok(Some(raw.clone())),
            Some(Value::Null) | None => Ok(None),
            Some(_) => Err(invalid(format!("`{field}` must be a string"))),
        }
    };
    let scope = string("scope")?.ok_or_else(|| invalid("ReadMemory requires `scope`".into()))?;
    let scope = parse_scope(&scope).map_err(|error| invalid(error.to_string()))?;
    let project = string("project")?;
    if let Some(why) = project
        .as_deref()
        .and_then(|project| project_problem(scope, project))
    {
        return Err(invalid(why));
    }
    let file = string("file")?
        .map(|file| read_file_name(&file))
        .transpose()?
        .flatten();
    Ok(ReadRequest {
        scope,
        project,
        file,
    })
}

/// The memory a `file` names, or `None` for the index.
///
/// Two spellings of the index are accepted on purpose: leaving `file` out is
/// what the schema describes, and `MEMORY.md` is what the index is called on
/// disk, so a caller that names it is asking for the same thing rather than
/// for a memory `SaveMemory` would refuse to create.
fn read_file_name(raw: &str) -> ToolResult<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(ENTRYPOINT_NAME) {
        return Ok(None);
    }
    validate_memory_file_name(trimmed)
        .map(Some)
        .map_err(|error| invalid(error.to_string()))
}

/// The memory directory `request` reads from.
fn memory_dir(request: &ReadRequest, context: &ToolContext) -> ToolResult<std::path::PathBuf> {
    let cwd =
        memory_cwd(request.scope, request.project.as_deref(), context.cwd()).map_err(invalid)?;
    memory_dir_for_scope(request.scope, cwd)
        .ok_or_else(|| invalid("no config home to find memory in".into()))
}

fn invalid(reason: String) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(READ_MEMORY_TOOL_NAME),
        reason,
        error_code: Some(INVALID_INPUT_CODE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::test_env::env_test_lock;
    use crate::SaveMemoryTool;

    /// The config home (and so every memory directory) points into a temp
    /// directory for as long as this lives.
    struct Home {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl Home {
        fn new() -> Self {
            let lock = env_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let temp = tempfile::tempdir().expect("temp home");
            let previous = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("REBON_CONFIG_DIR", temp.path().join(".rebon-test"));
            Self {
                _lock: lock,
                _temp: temp,
                previous,
            }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
        }
    }

    fn router(workspace: &tempfile::TempDir) -> ToolContext {
        ToolContext::new().with_cwd(workspace.path().to_string_lossy().into_owned())
    }

    async fn save_lesson(project: &str, context: &ToolContext) -> String {
        let saved = SaveMemoryTool
            .call(
                json!({
                    "action": "upsert",
                    "scope": "repo",
                    "type": "project",
                    "title": "Verify with cargo test",
                    "description": "Checks run cargo test -p calc",
                    "content": "Run `cargo test -p calc` before calling a change done.",
                    "project": project,
                }),
                context,
            )
            .await
            .unwrap();
        std::path::Path::new(saved["memory_path"].as_str().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn the_router_reads_a_projects_index_and_one_memory_it_lists() {
        let _home = Home::new();
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().into_owned();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);
        let file = save_lesson(&project_path, &context).await;

        let index = ReadMemoryTool
            .call(json!({"scope": "repo", "project": project_path}), &context)
            .await
            .unwrap();
        let index = index["content"].as_str().unwrap();
        assert!(index.contains("Checks run cargo test -p calc"), "{index}");
        assert!(index.contains(&file), "{index}");

        let memory = ReadMemoryTool
            .call(
                json!({"scope": "repo", "project": project_path, "file": file}),
                &context,
            )
            .await
            .unwrap();
        assert!(memory["content"]
            .as_str()
            .unwrap()
            .contains("Run `cargo test -p calc`"));
    }

    #[tokio::test]
    async fn an_empty_memory_says_so_and_a_missing_memory_is_an_error() {
        let _home = Home::new();
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().into_owned();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);

        let index = ReadMemoryTool
            .call(json!({"scope": "repo", "project": project_path}), &context)
            .await
            .unwrap();
        assert_eq!(index["content"], "No memory is saved here yet.");

        let error = ReadMemoryTool
            .call(
                json!({"scope": "repo", "project": project_path, "file": "gone.md"}),
                &context,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read the index"), "{error}");
    }

    #[tokio::test]
    async fn it_reads_only_memory_files_and_takes_project_on_save_memorys_terms() {
        let _home = Home::new();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);
        for (input, expected) in [
            (
                json!({"scope": "repo", "file": "../secrets.md"}),
                "basename",
            ),
            (json!({"scope": "repo", "file": "dir/x.md"}), "basename"),
            (json!({"scope": "repo", "file": "notes.txt"}), ".md"),
            (
                json!({"scope": "user", "project": "/somewhere"}),
                "only with scope `repo`",
            ),
            (
                json!({"scope": "repo", "project": "relative"}),
                "absolute path",
            ),
            (json!({"scope": "task"}), "storage scope"),
            (json!({}), "requires `scope`"),
            (json!({"scope": 3}), "`scope` must be a string"),
            (
                json!({"scope": "user", "file": 7}),
                "`file` must be a string",
            ),
            (
                json!({"scope": "user", "project": false}),
                "`project` must be a string",
            ),
        ] {
            let outcome = ReadMemoryTool
                .validate_input(&input, &context)
                .await
                .unwrap();
            assert!(!outcome.is_valid(), "{input}");
            let message = outcome.message.unwrap();
            assert!(message.contains(expected), "{input}: {message}");
        }
        let missing = workspace.path().join("gone");
        let error = ReadMemoryTool
            .call(
                json!({"scope": "repo", "project": missing.to_string_lossy()}),
                &context,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    /// What the model is told it may send and what the tool accepts have to
    /// name the same inputs: the index is reached by leaving `file` out, by
    /// passing null, or by spelling the index's own name — and `project` is
    /// nullable because a strict schema must be able to require it while
    /// scope `user` still refuses it.
    #[tokio::test]
    async fn the_schema_and_the_validator_agree_on_every_documented_spelling() {
        let _home = Home::new();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);
        let schema = ReadMemoryTool.input_schema();
        assert_eq!(schema["required"], json!(["scope"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["scope"]["type"], "string");
        for field in ["project", "file"] {
            assert_eq!(
                schema["properties"][field]["type"],
                json!(["string", "null"]),
                "`{field}` must be nullable so a strict schema can list it as required"
            );
        }

        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().into_owned();
        let file = save_lesson(&project_path, &context).await;
        for index_input in [
            json!({"scope": "repo", "project": project_path}),
            json!({"scope": "repo", "project": project_path, "file": null}),
            json!({"scope": "repo", "project": project_path, "file": ""}),
            json!({"scope": "repo", "project": project_path, "file": "MEMORY.md"}),
            json!({"scope": "repo", "project": project_path, "file": "memory.md"}),
            json!({"scope": "repo", "project": project_path, "file": "  "}),
        ] {
            assert!(
                ReadMemoryTool
                    .validate_input(&index_input, &context)
                    .await
                    .unwrap()
                    .is_valid(),
                "{index_input}"
            );
            let read = ReadMemoryTool
                .call(index_input.clone(), &context)
                .await
                .unwrap();
            let content = read["content"].as_str().unwrap();
            assert!(content.contains(&file), "{index_input}: {content}");
        }

        let memory = ReadMemoryTool
            .call(
                json!({"scope": "repo", "project": project_path, "file": file}),
                &context,
            )
            .await
            .unwrap();
        assert!(memory["content"]
            .as_str()
            .unwrap()
            .contains("Run `cargo test -p calc`"));
    }

    /// Every property the schema declares is accepted by the runtime
    /// validator, in the exact shape the schema describes — the check that
    /// caught the contract drifting from the tool.
    #[tokio::test]
    async fn the_exposed_contract_accepts_the_calls_it_documents() {
        use rebon_core::Engine;
        use rebon_tool::validation::validate_schema;

        let _home = Home::new();
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().into_owned();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);
        let file = save_lesson(&project_path, &context).await;

        let mut engine = Engine::new();
        engine.register_tool(std::sync::Arc::new(ReadMemoryTool));
        let schema = engine.tool_snapshots()[0].input_schema.clone();
        for call in [
            json!({"scope": "user"}),
            json!({"scope": "user", "project": null, "file": null}),
            json!({"scope": "user", "file": "MEMORY.md"}),
            json!({"scope": "repo", "project": project_path}),
            json!({"scope": "repo", "project": project_path, "file": ""}),
            json!({"scope": "repo", "project": project_path, "file": file}),
            json!({"scope": "repo", "project": null, "file": null}),
        ] {
            validate_schema(READ_MEMORY_TOOL_NAME, &schema, &call)
                .unwrap_or_else(|error| panic!("{call} is refused by its own schema: {error:?}"));
            engine
                .invoke_tool(READ_MEMORY_TOOL_NAME, call.clone(), &context)
                .await
                .unwrap_or_else(|error| panic!("{call} is refused at dispatch: {error}"));
        }

        for call in [
            json!({}),
            json!({"scope": "task"}),
            json!({"scope": "user", "project": project_path}),
            json!({"scope": "repo", "project": "relative"}),
            json!({"scope": "repo", "file": "../x.md"}),
            json!({"scope": "user", "file": "notes.txt"}),
            json!({"scope": "user", "unknown": true}),
        ] {
            let error = engine
                .invoke_tool(READ_MEMORY_TOOL_NAME, call.clone(), &context)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    rebon_tools_core::ToolError::InvalidInput {
                        error_code: Some(400),
                        ..
                    }
                ),
                "{call}: {error:?}"
            );
        }
    }

    /// A provider whose strict mode rewrites the schema to require every
    /// declared property must still be answerable: the nullable pair is what
    /// makes `scope`-only calls expressible once `file` and `project` are
    /// forced present.
    ///
    /// This is the shape the live router hit — all three names in `required`.
    /// The tool cannot stop a provider from rewriting the schema, so what it
    /// must guarantee is that the rewritten schema has a legal call for every
    /// documented use.
    #[tokio::test]
    async fn a_strict_schema_that_requires_every_property_still_has_a_legal_call() {
        use rebon_tool::validation::validate_schema;

        let _home = Home::new();
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().into_owned();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);
        let file = save_lesson(&project_path, &context).await;

        let mut strict = ReadMemoryTool.input_schema();
        strict["required"] = json!(["scope", "project", "file"]);
        assert_eq!(
            strict["properties"]["project"]["type"],
            json!(["string", "null"])
        );

        for call in [
            json!({"scope": "user", "project": null, "file": null}),
            json!({"scope": "user", "project": null, "file": "MEMORY.md"}),
            json!({"scope": "repo", "project": project_path, "file": null}),
            json!({"scope": "repo", "project": null, "file": null}),
            json!({"scope": "repo", "project": project_path, "file": file}),
        ] {
            validate_schema(READ_MEMORY_TOOL_NAME, &strict, &call).unwrap_or_else(|error| {
                panic!("{call} is refused by the strict schema: {error:?}")
            });
            ReadMemoryTool
                .call(call.clone(), &context)
                .await
                .unwrap_or_else(|error| panic!("{call} is refused at call time: {error}"));
        }

        // The rewrite must not turn the nullable pair into a licence for any
        // value: a strict schema still refuses an integer where a string or
        // null is declared.
        for call in [
            json!({"scope": "user", "project": 7, "file": null}),
            json!({"scope": "user", "project": null, "file": false}),
        ] {
            validate_schema(READ_MEMORY_TOOL_NAME, &strict, &call)
                .expect_err("a strict schema must still reject a wrong-typed value");
            let error = ReadMemoryTool
                .call(call.clone(), &context)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    rebon_tools_core::ToolError::InvalidInput {
                        error_code: Some(400),
                        ..
                    }
                ),
                "{call}: {error:?}"
            );
        }
    }

    /// The router has no cwd of its own to speak of: user memory must be
    /// readable without one, and repo memory must say what it needs.
    #[tokio::test]
    async fn user_memory_needs_no_cwd_and_repo_memory_says_what_it_needs() {
        let _home = Home::new();
        let workspace = tempfile::tempdir().unwrap();
        let seeded = router(&workspace);
        SaveMemoryTool
            .call(
                json!({
                    "action": "upsert",
                    "scope": "user",
                    "type": "feedback",
                    "title": "Prefer terse replies",
                    "description": "The person wants the short version",
                    "content": "Answer briefly.",
                }),
                &seeded,
            )
            .await
            .unwrap();

        let context = ToolContext::new();
        let index = ReadMemoryTool
            .call(json!({"scope": "user"}), &context)
            .await
            .unwrap();
        let index = index["content"].as_str().unwrap();
        assert!(
            index.contains("The person wants the short version"),
            "{index}"
        );
        let file = index
            .split("](")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .expect("the index lists the memory it holds")
            .to_string();
        let memory = ReadMemoryTool
            .call(json!({"scope": "user", "file": file}), &context)
            .await
            .unwrap();
        assert!(memory["content"]
            .as_str()
            .unwrap()
            .contains("Answer briefly."));

        let error = ReadMemoryTool
            .call(json!({"scope": "repo"}), &context)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cwd_missing"), "{error}");
        assert!(error.to_string().contains("pass `project`"), "{error}");
    }

    /// The project a session runs in is what repo memory falls back to, and
    /// naming `project` explicitly reads that project instead.
    #[tokio::test]
    async fn repo_memory_reads_the_sessions_project_or_the_one_it_names() {
        let _home = Home::new();
        let session = tempfile::tempdir().unwrap();
        let named = tempfile::tempdir().unwrap();
        let session_path = session.path().to_string_lossy().into_owned();
        let named_path = named.path().to_string_lossy().into_owned();
        let context = router(&session);

        SaveMemoryTool
            .call(
                json!({
                    "action": "upsert",
                    "scope": "repo",
                    "type": "project",
                    "title": "Session project",
                    "description": "Lives where the session runs",
                    "content": "Session body.",
                }),
                &context,
            )
            .await
            .unwrap();
        SaveMemoryTool
            .call(
                json!({
                    "action": "upsert",
                    "scope": "repo",
                    "type": "project",
                    "title": "Named project",
                    "description": "Lives where project points",
                    "content": "Named body.",
                    "project": named_path,
                }),
                &context,
            )
            .await
            .unwrap();

        let own = ReadMemoryTool
            .call(json!({"scope": "repo"}), &context)
            .await
            .unwrap();
        let own = own["content"].as_str().unwrap();
        assert!(own.contains("Lives where the session runs"), "{own}");
        assert!(!own.contains("Lives where project points"), "{own}");

        let other = ReadMemoryTool
            .call(json!({"scope": "repo", "project": named_path}), &context)
            .await
            .unwrap();
        let other = other["content"].as_str().unwrap();
        assert!(other.contains("Lives where project points"), "{other}");
        assert!(!other.contains("Lives where session runs"), "{other}");
        assert_ne!(session_path, named_path);
    }

    #[test]
    fn a_long_memory_is_cut_and_says_so() {
        let short = "a".repeat(MAX_CHARS);
        assert_eq!(clip(short.clone()), short);
        let long = clip("b".repeat(MAX_CHARS + 10));
        assert!(long.starts_with(&"b".repeat(MAX_CHARS)));
        assert!(long.ends_with("longer than ReadMemory returns]"));
    }

    #[test]
    fn it_is_read_only_and_asks_for_no_permission() {
        let input = json!({"scope": "user"});
        assert!(ReadMemoryTool.is_read_only(&input));
        assert!(!ReadMemoryTool.needs_permission(&input));
        assert_eq!(ReadMemoryTool.input_schema()["required"], json!(["scope"]));
    }
}
