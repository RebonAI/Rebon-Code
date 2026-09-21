//! `AskUserQuestion` — prompts the user with multiple-choice
//! questions to gather preferences, clarify ambiguity, or pick between
//! approaches mid-turn.
//!
//! The tool takes the full input shape (1–4 questions, 2–4
//! options per question, `multiSelect`, `preview`, `annotations`,
//! `metadata`) with the uniqueness validation contract. User
//! interaction is deferred to the harness via
//! [`PermissionDecision::ask`] — the broker renders the dialog,
//! collects answers, and returns them via `updated_input.answers`.
//! [`AskUserQuestionTool::call`] then echoes the questions alongside
//! the collected answers so downstream dispatch can hand a
//! well-formed result block back to the model.
//!
//! ## Input fields
//!
//! * `questions[i].question` — full question text, shown above the chips.
//! * `questions[i].header` — short chip label, max 12 chars.
//! * `questions[i].options[j].label` — short choice label.
//! * `questions[i].options[j].description` — explanation of the choice.
//! * `questions[i].options[j].preview` — optional preview
//!   (markdown/HTML) rendered for side-by-side comparison. Only valid
//!   for single-select questions.
//! * `questions[i].multiSelect` — defaults to `false`.
//! * `answers` — populated by the broker: `question text → answer`.
//! * `annotations` — populated by the broker: `question text → { preview?, notes? }`.
//! * `metadata.source` — opaque caller-supplied string; nothing reads it.

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, PermissionDecision, PermissionRequest, ToolError, ToolId,
    ToolInputSchema, ToolResult, ValidationOutcome,
};
use rebon_types::{
    ultraplan_plan_hash_for_profile, UltraplanProfile, ULTRAPLAN_CONFIRM_UNDERSTANDING_INTENT,
    ULTRAPLAN_CONFIRM_UNDERSTANDING_LABEL, ULTRAPLAN_REVISE_UNDERSTANDING_LABEL,
};

pub const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";
const INVALID_INPUT_CODE: i64 = 400;

/// The maximum chip-label width in characters.
const ASK_USER_QUESTION_TOOL_CHIP_WIDTH: usize = 12;

/// Minimum
/// number of questions per call.
const MIN_QUESTIONS: usize = 1;
/// Maximum
/// number of questions per call.
const MAX_QUESTIONS: usize = 4;
/// Each question must have at least 2 options.
const MIN_OPTIONS: usize = 2;
/// Each question must have at most 4 options.
const MAX_OPTIONS: usize = 4;

#[derive(Debug, Clone, Default)]
pub struct AskUserQuestionTool;

#[derive(Debug, Clone)]
struct AskUserQuestionInput {
    questions: Vec<Question>,
    /// Answers populated by the permission broker. `question text →
    /// answer string` (multi-select answers are comma-joined).
    answers: Option<Map<String, Value>>,
    /// Optional annotations from the broker:
    /// `question text → { preview?, notes? }`.
    annotations: Option<Map<String, Value>>,
    /// Optional caller metadata (opaque blob). Parsed so the input is
    /// accepted as sent; the Grill gate reads `intent`, `plan_hash` and
    /// `interview_revision` from it.
    metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone)]
struct Question {
    question: String,
    header: String,
    options: Vec<QuestionOption>,
    multi_select: bool,
}

#[derive(Debug, Clone)]
struct QuestionOption {
    label: String,
    #[allow(dead_code)]
    description: String,
    preview: Option<String>,
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn id(&self) -> ToolId {
        ToolId::new(ASK_USER_QUESTION_TOOL_NAME)
    }

    // No plan-mode paragraph here. What this tool may and may not be used for
    // while planning is plan mode's to say, and its reminders say it on every
    // poll that matters — see `rebon_plugin_plan_mode::attachments`. Kept here
    // it shipped plan vocabulary into every session that has this tool,
    // planning or not, which is most of them.
    fn description(&self) -> &str {
        "Asks the user multiple-choice questions only when an unresolved user decision blocks progress.\n\
         \n\
         Before calling this tool, investigate anything the codebase or available context can answer. \
         Combine all foreseeable independent blocking decisions into one call, up to the four-question limit. \
         Stop asking as soon as the user's answers are sufficient to proceed.\n\
         \n\
         Do not use this tool for notifications, operation instructions, thanks, restatements, \
         acknowledging that you received or understood a message, or closing remarks. Communicate \
         those directly in normal assistant text instead of packaging them as questions.\n\
         \n\
         Usage notes:\n\
         - Users will always be able to select \"Other\" to provide custom text input\n\
         - Use multiSelect: true to allow multiple answers to be selected for a question\n\
         - If you recommend a specific option, make that the first option in the list and \
         add \"(Recommended)\" at the end of the label"
    }

    fn model_description(&self) -> &str {
        "Asks multiple-choice questions only for unresolved user decisions that block progress. Batch foreseeable questions, stop when answers are sufficient, and never use it for notifications or closing remarks."
    }

    fn input_schema(&self) -> ToolInputSchema {
        // Input schema for the tool's JSON input.
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": MIN_QUESTIONS,
                    "maxItems": MAX_QUESTIONS,
                    "description": "Questions to ask (1-4).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "Full question text."
                            },
                            "header": {
                                "type": "string",
                                "maxLength": ASK_USER_QUESTION_TOOL_CHIP_WIDTH,
                                "description": format!("Short chip label (max {ASK_USER_QUESTION_TOOL_CHIP_WIDTH} characters).")
                            },
                            "options": {
                                "type": "array",
                                "minItems": MIN_OPTIONS,
                                "maxItems": MAX_OPTIONS,
                                "description": "Two to four choices; Other is added automatically.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Short choice label."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Choice explanation."
                                        },
                                        "preview": {
                                            "type": "string",
                                            "description": "Optional single-select preview."
                                        }
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "default": false,
                                "description": "Allow multiple choices."
                            }
                        },
                        "required": ["question", "header", "options"],
                        "additionalProperties": false
                    }
                },
                "answers": {
                    "type": "object",
                    "description": "Broker-collected answers.",
                    "additionalProperties": { "type": "string" }
                },
                "annotations": {
                    "type": "object",
                    "description": "Broker-collected previews or notes.",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "preview": { "type": "string" },
                            "notes": { "type": "string" }
                        },
                        "additionalProperties": false
                    }
                },
                "metadata": {
                    "type": "object",
                    "description": "Optional tracking and Grill gate data.",
                    "properties": {
                        "source": {
                            "type": "string",
                            "description": "Tracking source."
                        },
                        "intent": {
                            "type": "string",
                            "description": "Use `confirm_understanding` for Grill confirmation."
                        },
                        "plan_hash": {
                            "type": "string",
                            "description": "Reviewed Grill plan hash."
                        },
                        "interview_revision": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Sealed Grill interview revision."
                        }
                    },
                    "additionalProperties": true
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        // The "permission" step IS the user dialog — the broker
        // renders the multiple-choice prompt, collects answers, and
        // feeds them back via `updated_input.answers`.
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(prepare_input(self.id(), input, context))
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        // The broker is expected to render the dialog and return the
        // same input object with an `answers` map populated.
        Ok(PermissionDecision::ask(
            PermissionRequest::new("Answer questions?", "Rebon is asking for your input."),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = prepare_input(self.id(), &input, context)?;

        // By the time `call` runs, the broker should have populated
        // `answers`. If it didn't, the tool was invoked bypassing the
        // approval path — surface that as an execution error instead
        // of silently returning an empty answer map.
        let answers = parsed.answers.clone().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("AskUserQuestion requires answers from the permission broker"),
        })?;

        // Serialise questions back out — downstream rendering and the
        // tool_result block both expect the full question definition
        // alongside the collected answers (the tool's output schema).
        let questions_out: Vec<Value> = parsed.questions.iter().map(question_to_json).collect();
        let annotations_out = parsed
            .annotations
            .clone()
            .map(Value::Object)
            .unwrap_or(Value::Null);

        Ok(json!({
            "questions": questions_out,
            "answers": Value::Object(answers),
            "annotations": annotations_out,
        }))
    }
}

fn question_to_json(question: &Question) -> Value {
    json!({
        "question": question.question,
        "header": question.header,
        "multiSelect": question.multi_select,
        "options": question
            .options
            .iter()
            .map(|opt| {
                let mut map = Map::new();
                map.insert("label".into(), Value::String(opt.label.clone()));
                map.insert("description".into(), Value::String(opt.description.clone()));
                if let Some(preview) = &opt.preview {
                    map.insert("preview".into(), Value::String(preview.clone()));
                }
                Value::Object(map)
            })
            .collect::<Vec<_>>(),
    })
}

fn enforce_uniqueness(questions: &[Question]) -> Result<(), String> {
    // Question texts must be unique across the call.
    let mut seen_questions = std::collections::HashSet::new();
    for q in questions {
        if !seen_questions.insert(q.question.as_str()) {
            return Err(
                "Question texts must be unique, option labels must be unique within each question"
                    .to_string(),
            );
        }
        // Option labels must be unique within each question.
        let mut seen_labels = std::collections::HashSet::new();
        for opt in &q.options {
            if !seen_labels.insert(opt.label.as_str()) {
                return Err(
                    "Question texts must be unique, option labels must be unique within each question"
                        .to_string(),
                );
            }
        }
    }
    Ok(())
}

fn enforce_grill_question_shape(
    input: &AskUserQuestionInput,
    context: &ToolContext,
) -> Result<(), String> {
    let grill_active = context
        .ultraplan_context()
        .is_some_and(|ultraplan| ultraplan.profile == UltraplanProfile::Grill);
    if !grill_active {
        return Ok(());
    }
    if input.questions.len() != 1 {
        return Err(
            "Grill-profile ultraplan requires exactly one AskUserQuestion question per call".into(),
        );
    }
    let question = &input.questions[0];
    if question.multi_select {
        return Err("Grill-profile ultraplan questions must be single-select".into());
    }

    let intent = input
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("intent"))
        .and_then(Value::as_str);
    let Some(intent) = intent else {
        return Ok(());
    };
    if intent != ULTRAPLAN_CONFIRM_UNDERSTANDING_INTENT {
        return Err(format!("unsupported Grill question intent `{intent}`"));
    }
    if question.options.len() != 2
        || question.options[0].label != ULTRAPLAN_CONFIRM_UNDERSTANDING_LABEL
        || question.options[1].label != ULTRAPLAN_REVISE_UNDERSTANDING_LABEL
    {
        return Err(format!(
            "Grill understanding confirmation requires exactly two options labeled `{ULTRAPLAN_CONFIRM_UNDERSTANDING_LABEL}` and `{ULTRAPLAN_REVISE_UNDERSTANDING_LABEL}`"
        ));
    }

    let metadata = input.metadata.as_ref().expect("intent requires metadata");
    let plan_hash = metadata
        .get("plan_hash")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "Grill understanding confirmation requires metadata.plan_hash".to_string()
        })?;
    let revision = metadata
        .get("interview_revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "Grill understanding confirmation requires metadata.interview_revision".to_string()
        })?;
    let state = context
        .load_ultraplan_run_state()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| {
            "Grill understanding confirmation requires an active ultraplan run state".to_string()
        })?;
    if !state.grill_understanding_is_sealed() || state.ledger_revision != revision {
        return Err(
            "Grill understanding confirmation references a stale or unsealed interview revision"
                .into(),
        );
    }
    // A degraded final gate satisfies the review requirement without an
    // auto-review PASS; the confirmation card is still mandatory.
    if !state.grill_plan_review_satisfied(plan_hash) {
        return Err(
            "Grill understanding confirmation requires the current auto-reviewed plan hash".into(),
        );
    }
    let draft = state.last_plan_draft.as_deref().ok_or_else(|| {
        "Grill understanding confirmation requires a persisted plan draft".to_string()
    })?;
    if ultraplan_plan_hash_for_profile(UltraplanProfile::Grill, draft) != plan_hash {
        return Err(
            "Grill understanding confirmation plan hash does not match the persisted draft".into(),
        );
    }
    let preview = question.options[0].preview.as_deref().ok_or_else(|| {
        "Grill understanding confirmation must show the reviewed draft in the Confirm option preview".to_string()
    })?;
    // The plan hash ignores `ULTRAPLAN_DRAFT_HASH:` marker lines, so a
    // preview containing them could show the user arbitrary extra text while
    // still hashing to the reviewed draft. Reject markers outright, then
    // compare by hash instead of byte equality (whitespace-normalized like
    // every other plan-hash binding).
    if preview
        .lines()
        .any(rebon_types::is_ultraplan_draft_hash_marker)
    {
        return Err(
            "Grill understanding confirmation preview must not contain ULTRAPLAN_DRAFT_HASH marker lines".into(),
        );
    }
    if ultraplan_plan_hash_for_profile(UltraplanProfile::Grill, preview) != plan_hash {
        return Err("Grill understanding confirmation must show the exact reviewed draft in the Confirm option preview".into());
    }
    Ok(())
}

/// Parse and vet one `AskUserQuestion` request, once.
///
/// Both entry points ask the same three questions in the same order: does the
/// input parse, are the questions distinct, does the request fit the grill
/// profile's shape.
fn prepare_input(
    tool: ToolId,
    input: &Value,
    context: &ToolContext,
) -> ToolResult<AskUserQuestionInput> {
    let parsed = parse_input(input)?;
    let refuse = |reason: String| ToolError::InvalidInput {
        tool: tool.clone(),
        reason,
        error_code: Some(INVALID_INPUT_CODE),
    };
    enforce_uniqueness(&parsed.questions).map_err(refuse)?;
    enforce_grill_question_shape(&parsed, context).map_err(refuse)?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<AskUserQuestionInput> {
    let tool = ToolId::new(ASK_USER_QUESTION_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "AskUserQuestion input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let questions_raw = object
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "AskUserQuestion requires an array `questions`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

    if questions_raw.len() < MIN_QUESTIONS || questions_raw.len() > MAX_QUESTIONS {
        return Err(ToolError::InvalidInput {
            tool,
            reason: format!("AskUserQuestion requires {MIN_QUESTIONS}-{MAX_QUESTIONS} questions"),
            error_code: Some(INVALID_INPUT_CODE),
        });
    }

    let questions = questions_raw
        .iter()
        .map(|raw| parse_question(raw, &tool))
        .collect::<ToolResult<Vec<_>>>()?;

    let answers = optional_string_map(object.get("answers"), "answers", &tool)?;
    let annotations = optional_map(object.get("annotations"), "annotations", &tool)?;
    let metadata = optional_map(object.get("metadata"), "metadata", &tool)?;

    Ok(AskUserQuestionInput {
        questions,
        answers,
        annotations,
        metadata,
    })
}

fn parse_question(raw: &Value, tool: &ToolId) -> ToolResult<Question> {
    let obj = raw.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Each question must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let question = required_string(obj.get("question"), "question", tool)?;
    let header = required_string(obj.get("header"), "header", tool)?;
    if header.chars().count() > ASK_USER_QUESTION_TOOL_CHIP_WIDTH {
        return Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!(
                "`header` must be at most {ASK_USER_QUESTION_TOOL_CHIP_WIDTH} characters"
            ),
            error_code: Some(INVALID_INPUT_CODE),
        });
    }

    let options_raw = obj
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "Each question requires an `options` array".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

    if options_raw.len() < MIN_OPTIONS || options_raw.len() > MAX_OPTIONS {
        return Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("Each question requires {MIN_OPTIONS}-{MAX_OPTIONS} options"),
            error_code: Some(INVALID_INPUT_CODE),
        });
    }

    let options = options_raw
        .iter()
        .map(|opt_raw| parse_option(opt_raw, tool))
        .collect::<ToolResult<Vec<_>>>()?;

    let multi_select = obj
        .get("multiSelect")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Preview is documented as single-select-only. Enforce it here
    // instead of silently ignoring the field.
    if multi_select && options.iter().any(|o| o.preview.is_some()) {
        return Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "Option previews are only supported for single-select questions".into(),
            error_code: Some(INVALID_INPUT_CODE),
        });
    }

    Ok(Question {
        question,
        header,
        options,
        multi_select,
    })
}

fn parse_option(raw: &Value, tool: &ToolId) -> ToolResult<QuestionOption> {
    let obj = raw.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Each option must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let label = required_string(obj.get("label"), "label", tool)?;
    let description = required_string(obj.get("description"), "description", tool)?;
    let preview = optional_string_field(obj.get("preview"), "preview", tool)?;

    Ok(QuestionOption {
        label,
        description,
        preview,
    })
}

fn required_string(value: Option<&Value>, field: &str, tool: &ToolId) -> ToolResult<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("AskUserQuestion requires a non-empty string `{field}`"),
            error_code: Some(INVALID_INPUT_CODE),
        })
}

fn optional_string_field(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<String>> {
    match value {
        Some(Value::String(raw)) => Ok(Some(raw.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be a string when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

fn optional_map(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<Map<String, Value>>> {
    match value {
        Some(Value::Object(map)) => Ok(Some(map.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be an object when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

/// Stricter variant: every value must be a string. Used for the
/// `answers` map (`question text → answer string`).
fn optional_string_map(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<Map<String, Value>>> {
    let map = match optional_map(value, field, tool)? {
        Some(map) => map,
        None => return Ok(None),
    };
    for (key, val) in &map {
        if !val.is_string() {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!("`{field}.{key}` must be a string"),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }
    }
    Ok(Some(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tools_core::PermissionBehavior;
    use std::sync::{Arc, Mutex};

    fn tool() -> AskUserQuestionTool {
        AskUserQuestionTool
    }

    fn base_question() -> Value {
        json!({
            "question": "Which auth backend should we use?",
            "header": "Auth",
            "options": [
                { "label": "Session", "description": "Stateful server sessions" },
                { "label": "JWT", "description": "Stateless tokens" }
            ],
        })
    }

    fn base_input() -> Value {
        json!({ "questions": [base_question()] })
    }

    fn grill_context() -> ToolContext {
        let context = rebon_types::UltraplanContext::planning_turn(
            "run",
            "plan",
            rebon_types::PolicyMode::Enforce,
        )
        .with_profile(UltraplanProfile::Grill);
        ToolContext::new().with_execution_policy(rebon_types::ExecutionPolicy::ultraplan(context))
    }

    fn grill_confirmation_context() -> (ToolContext, String, u64, String) {
        let plan = "P1. Ship it".to_string();
        let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Grill, &plan);
        let mut state = rebon_types::UltraplanRunState::new(
            "run".into(),
            "session".into(),
            "task".into(),
            None,
            1,
        )
        .with_profile(UltraplanProfile::Grill);
        state
            .requirement_ledger
            .push(rebon_types::RequirementLedgerEntry {
                id: "R1".into(),
                title: "Ship safely".into(),
                source: rebon_types::RequirementSource::Question,
                round_added: 1,
            });
        state.record_grill_interview_turn(
            "Which rollout?".into(),
            Some("Gradual".into()),
            "Gradual".into(),
        );
        state.seal_grill_understanding().unwrap();
        state.last_plan_draft = Some(plan.clone());
        state.auto_review_passed_hash = Some(plan_hash.clone());
        let revision = state.interview.revision;
        let handle = Arc::new(Mutex::new(state));
        let context = grill_context().with_ultraplan_run_handle(handle);
        (context, plan, revision, plan_hash)
    }

    fn grill_confirmation_input(plan: &str, revision: u64, plan_hash: &str) -> Value {
        json!({
            "questions": [{
                "question": "Does this exact reviewed draft match our shared understanding?",
                "header": "Confirm plan",
                "options": [
                    {
                        "label": ULTRAPLAN_CONFIRM_UNDERSTANDING_LABEL,
                        "description": "Confirm the requirements and draft are accurate.",
                        "preview": plan
                    },
                    {
                        "label": ULTRAPLAN_REVISE_UNDERSTANDING_LABEL,
                        "description": "Return to the interview and revise the draft."
                    }
                ]
            }],
            "metadata": {
                "intent": ULTRAPLAN_CONFIRM_UNDERSTANDING_INTENT,
                "plan_hash": plan_hash,
                "interview_revision": revision
            }
        })
    }

    #[test]
    fn prompt_contract_rejects_non_decision_messages() {
        let tool = tool();
        let description = tool.description();
        for needle in [
            "unresolved user decision blocks progress",
            "Combine all foreseeable independent blocking decisions",
            "Stop asking as soon as the user's answers are sufficient",
            "Do not use this tool for notifications",
            "operation instructions",
            "thanks",
            "restatements",
            "acknowledging that you received or understood a message",
            "closing remarks",
        ] {
            assert!(
                description.contains(needle),
                "missing {needle:?} in description:\n{description}"
            );
        }

        let model_description = tool.model_description();
        assert!(
            model_description.contains("only for unresolved user decisions that block progress")
        );
        assert!(model_description.contains("stop when answers are sufficient"));
        assert!(model_description.contains("never use it for notifications or closing remarks"));
    }

    /// Neither description carries plan-mode vocabulary. This tool ships in
    /// nearly every session, and most of them never plan; what it may be used
    /// for while planning is said by plan mode's own reminders, which only a
    /// planning session reads.
    #[test]
    fn no_plan_vocabulary_reaches_a_session_that_is_not_planning() {
        let tool = tool();
        for (label, text) in [
            ("description", tool.description()),
            ("model_description", tool.model_description()),
        ] {
            for needle in [
                "Plan mode",
                "plan mode",
                "ExitPlanMode",
                "Grill",
                "confirm_understanding",
            ] {
                assert!(
                    !text.contains(needle),
                    "{label} still carries {needle:?}:\n{text}"
                );
            }
        }
    }

    #[tokio::test]
    async fn validate_rejects_non_object_input() {
        let outcome = tool()
            .validate_input(&json!("oops"), &ToolContext::new())
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert_eq!(outcome.error_code, Some(INVALID_INPUT_CODE));
    }

    #[tokio::test]
    async fn validate_rejects_empty_questions_array() {
        let outcome = tool()
            .validate_input(&json!({ "questions": [] }), &ToolContext::new())
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_more_than_four_questions() {
        let questions: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "question": format!("Q{i}?"),
                    "header": format!("H{i}"),
                    "options": [
                        { "label": "A", "description": "a" },
                        { "label": "B", "description": "b" }
                    ]
                })
            })
            .collect();
        let outcome = tool()
            .validate_input(&json!({ "questions": questions }), &ToolContext::new())
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn grill_profile_requires_one_single_select_question() {
        let second = json!({
            "question": "Which rollout strategy?",
            "header": "Rollout",
            "options": [
                { "label": "Gradual", "description": "Roll out in stages" },
                { "label": "Immediate", "description": "Ship at once" }
            ]
        });
        let outcome = tool()
            .validate_input(
                &json!({ "questions": [base_question(), second] }),
                &grill_context(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert!(outcome.message.unwrap().contains("exactly one"));

        let mut multi = base_input();
        multi["questions"][0]["multiSelect"] = json!(true);
        let outcome = tool()
            .validate_input(&multi, &grill_context())
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert!(outcome.message.unwrap().contains("single-select"));
    }

    #[tokio::test]
    async fn grill_confirmation_requires_current_revision_hash_and_exact_preview() {
        let (context, plan, revision, plan_hash) = grill_confirmation_context();
        let valid = grill_confirmation_input(&plan, revision, &plan_hash);
        assert!(tool()
            .validate_input(&valid, &context)
            .await
            .unwrap()
            .is_valid());

        let stale = grill_confirmation_input(&plan, revision + 1, &plan_hash);
        let outcome = tool().validate_input(&stale, &context).await.unwrap();
        assert!(!outcome.is_valid());
        assert!(outcome.message.unwrap().contains("stale or unsealed"));

        let mut wrong_preview = grill_confirmation_input(&plan, revision, &plan_hash);
        wrong_preview["questions"][0]["options"][0]["preview"] = json!("different draft");
        let outcome = tool()
            .validate_input(&wrong_preview, &context)
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert!(outcome.message.unwrap().contains("exact reviewed draft"));
    }

    #[tokio::test]
    async fn grill_confirmation_call_accepts_broker_answer_after_validation() {
        let (context, plan, revision, plan_hash) = grill_confirmation_context();
        let mut input = grill_confirmation_input(&plan, revision, &plan_hash);
        input["answers"] = json!({
            "Does this exact reviewed draft match our shared understanding?": ULTRAPLAN_CONFIRM_UNDERSTANDING_LABEL
        });

        let output = tool().call(input, &context).await.unwrap();
        assert_eq!(
            output["answers"]["Does this exact reviewed draft match our shared understanding?"],
            ULTRAPLAN_CONFIRM_UNDERSTANDING_LABEL
        );
    }

    #[tokio::test]
    async fn standard_profile_still_accepts_multiple_questions() {
        let second = json!({
            "question": "Which rollout strategy?",
            "header": "Rollout",
            "options": [
                { "label": "Gradual", "description": "Roll out in stages" },
                { "label": "Immediate", "description": "Ship at once" }
            ]
        });
        let outcome = tool()
            .validate_input(
                &json!({ "questions": [base_question(), second] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_option_below_two() {
        let outcome = tool()
            .validate_input(
                &json!({
                    "questions": [{
                        "question": "Pick",
                        "header": "H",
                        "options": [{ "label": "Only", "description": "d" }]
                    }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_option_above_four() {
        let options: Vec<Value> = (0..5)
            .map(|i| json!({ "label": format!("L{i}"), "description": "d" }))
            .collect();
        let outcome = tool()
            .validate_input(
                &json!({
                    "questions": [{
                        "question": "Pick",
                        "header": "H",
                        "options": options
                    }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_header_exceeding_chip_width() {
        let outcome = tool()
            .validate_input(
                &json!({
                    "questions": [{
                        "question": "Q?",
                        "header": "this-is-way-too-long",
                        "options": [
                            { "label": "A", "description": "d" },
                            { "label": "B", "description": "d" }
                        ]
                    }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_duplicate_question_texts() {
        let outcome = tool()
            .validate_input(
                &json!({
                    "questions": [base_question(), base_question()]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        let msg = outcome.message.unwrap();
        assert!(msg.contains("unique"));
    }

    #[tokio::test]
    async fn validate_rejects_duplicate_option_labels_within_question() {
        let outcome = tool()
            .validate_input(
                &json!({
                    "questions": [{
                        "question": "Q?",
                        "header": "H",
                        "options": [
                            { "label": "Same", "description": "d" },
                            { "label": "Same", "description": "d" }
                        ]
                    }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_preview_on_multi_select() {
        let outcome = tool()
            .validate_input(
                &json!({
                    "questions": [{
                        "question": "Q?",
                        "header": "H",
                        "multiSelect": true,
                        "options": [
                            { "label": "A", "description": "d", "preview": "```rust\nfn a() {}\n```" },
                            { "label": "B", "description": "d" }
                        ]
                    }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert!(outcome.message.unwrap().contains("single-select"));
    }

    #[tokio::test]
    async fn validate_accepts_well_formed_input() {
        let outcome = tool()
            .validate_input(&base_input(), &ToolContext::new())
            .await
            .unwrap();
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn check_permissions_returns_ask() {
        let decision = tool()
            .check_permissions(&base_input(), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        assert_eq!(
            decision.request.expect("ask carries request").title,
            "Answer questions?"
        );
    }

    #[tokio::test]
    async fn call_requires_answers_from_broker() {
        let err = tool()
            .call(base_input(), &ToolContext::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution { .. }));
    }

    #[tokio::test]
    async fn call_echoes_questions_and_answers() {
        let input = json!({
            "questions": [base_question()],
            "answers": {
                "Which auth backend should we use?": "JWT"
            }
        });
        let out = tool().call(input, &ToolContext::new()).await.unwrap();
        assert_eq!(
            out["answers"]["Which auth backend should we use?"],
            json!("JWT")
        );
        assert_eq!(out["questions"][0]["header"], json!("Auth"));
        assert_eq!(out["questions"][0]["multiSelect"], json!(false));
        assert_eq!(out["questions"][0]["options"][0]["label"], json!("Session"));
        assert_eq!(out["annotations"], Value::Null);
    }

    #[tokio::test]
    async fn call_surfaces_annotations_when_present() {
        let input = json!({
            "questions": [base_question()],
            "answers": { "Which auth backend should we use?": "JWT" },
            "annotations": {
                "Which auth backend should we use?": {
                    "notes": "Prefer JWT because most clients are mobile."
                }
            }
        });
        let out = tool().call(input, &ToolContext::new()).await.unwrap();
        assert!(out["annotations"]
            .get("Which auth backend should we use?")
            .is_some());
    }

    #[tokio::test]
    async fn call_roundtrips_preview_on_single_select() {
        let input = json!({
            "questions": [{
                "question": "Which layout?",
                "header": "Layout",
                "options": [
                    {
                        "label": "Stacked",
                        "description": "Top-to-bottom",
                        "preview": "┌───┐\n│ A │\n├───┤\n│ B │\n└───┘"
                    },
                    { "label": "Side by side", "description": "Left-right" }
                ]
            }],
            "answers": { "Which layout?": "Stacked" }
        });
        let out = tool().call(input, &ToolContext::new()).await.unwrap();
        assert_eq!(
            out["questions"][0]["options"][0]["preview"]
                .as_str()
                .unwrap(),
            "┌───┐\n│ A │\n├───┤\n│ B │\n└───┘"
        );
    }

    #[test]
    fn tool_surface_flags_are_expected() {
        assert_eq!(tool().id().as_str(), "AskUserQuestion");
        assert!(tool().is_read_only(&json!({})));
        assert!(!tool().is_concurrency_safe(&json!({})));
        assert!(tool().needs_permission(&json!({})));
    }

    #[test]
    fn input_schema_declares_expected_shape() {
        let schema = tool().input_schema();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["required"], json!(["questions"]));
        assert_eq!(schema["properties"]["questions"]["minItems"], json!(1));
        assert_eq!(schema["properties"]["questions"]["maxItems"], json!(4));
        let items = &schema["properties"]["questions"]["items"];
        assert_eq!(items["properties"]["options"]["minItems"], json!(2));
        assert_eq!(items["properties"]["options"]["maxItems"], json!(4));
    }

    #[tokio::test]
    async fn maximum_question_option_and_unicode_header_boundaries_are_preserved() {
        let questions: Vec<Value> = (0..MAX_QUESTIONS)
            .map(|index| {
                json!({
                    "question": format!("Question {index}?"),
                    "header": "问".repeat(ASK_USER_QUESTION_TOOL_CHIP_WIDTH),
                    "options": (0..MAX_OPTIONS).map(|option| json!({
                        "label": format!("Option {option}"),
                        "description": "Choose this option"
                    })).collect::<Vec<_>>()
                })
            })
            .collect();
        let mut input =
            json!({"questions": questions, "answers": {}, "annotations": null, "metadata": null});
        assert!(tool()
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap()
            .is_valid());
        let output = tool()
            .call(input.clone(), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(output["questions"].as_array().unwrap().len(), MAX_QUESTIONS);
        assert_eq!(
            output["questions"][0]["options"].as_array().unwrap().len(),
            MAX_OPTIONS
        );
        assert_eq!(output["answers"], json!({}));
        input["questions"][0]["header"] = json!("问".repeat(ASK_USER_QUESTION_TOOL_CHIP_WIDTH + 1));
        assert!(!tool()
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap()
            .is_valid());
        assert!(matches!(
            tool().call(input, &ToolContext::new()).await,
            Err(ToolError::InvalidInput {
                error_code: Some(400),
                ..
            })
        ));
    }

    #[test]
    fn metadata_source_is_parsed_without_error() {
        // Doesn't affect output, but ensures the field is optional and
        // tolerated by parse_input. Uses `parse_input` directly since
        // there's no public accessor on AskUserQuestionInput.
        let raw = json!({
            "questions": [base_question()],
            "metadata": { "source": "remember" }
        });
        let parsed = parse_input(&raw).expect("parse");
        assert!(parsed.metadata.is_some());
        assert_eq!(
            parsed.metadata.unwrap()["source"].as_str().unwrap(),
            "remember"
        );
    }
}
