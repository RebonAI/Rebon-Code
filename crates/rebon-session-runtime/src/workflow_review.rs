//! Turning a workflow-review permission query into text a person can read.
//!
//! Three pure functions: the fallback graph built from a raw message when
//! the tool sent no structured metadata, the overview rendered from that
//! metadata, and the revision feedback handed back to the engine when a
//! reviewer edits the graph instead of approving it. None of them touch
//! the terminal, and none of them decide anything — they only format.

use serde_json::Value;

pub fn workflow_review_graph_from_message(
    message: &str,
) -> Option<rebon_tools_core::WorkflowGraph> {
    let graph = rebon_tools_core::WorkflowGraph {
        fallback_text: Some(message.to_string()),
        ..Default::default()
    };
    Some(graph)
}

pub fn workflow_review_summary_from_metadata(metadata: Option<&Value>) -> Option<String> {
    let metadata = metadata?.as_object()?;
    if metadata
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "workflowReview")
    {
        return None;
    }

    let name = metadata
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("workflow");
    let title = metadata
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let description = metadata
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let source = metadata
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let args_summary = metadata
        .get("argsSummary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let agent_call_count = metadata
        .get("agentCallCount")
        .and_then(Value::as_u64)
        .or_else(|| {
            metadata
                .get("calls")
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .filter(|call| call.get("kind").and_then(Value::as_str) == Some("agent"))
                        .count() as u64
                })
        })
        .unwrap_or(0);
    let total_call_count = metadata
        .get("totalCallCount")
        .and_then(Value::as_u64)
        .or_else(|| {
            metadata
                .get("calls")
                .and_then(Value::as_array)
                .map(|calls| calls.len() as u64)
        })
        .unwrap_or(agent_call_count);

    let mut out = String::new();
    out.push_str("Overview\n");
    out.push_str("- Name: ");
    out.push_str(name);
    out.push('\n');
    if let Some(title) = title {
        out.push_str("- Title: ");
        out.push_str(title);
        out.push('\n');
    }
    if let Some(description) = description {
        out.push_str("- Summary: ");
        out.push_str(description);
        out.push('\n');
    } else {
        out.push_str("- Summary: (missing; review metadata is incomplete)\n");
    }
    if let Some(source) = source {
        out.push_str("- Source: ");
        out.push_str(source);
        out.push('\n');
    }
    out.push_str("- Args: ");
    out.push_str(args_summary.unwrap_or("(none)"));
    out.push('\n');
    out.push_str(&format!(
        "- Calls: {agent_call_count} agent call(s), {total_call_count} total static call(s)\n"
    ));

    out.push_str("\nPhases\n");
    let phases = metadata
        .get("phases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if phases.is_empty() {
        out.push_str("- (no phases declared in meta)\n");
    } else {
        for (idx, phase) in phases.iter().enumerate() {
            let title = phase
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("(untitled phase)");
            out.push_str(&format!("{}. {}", idx + 1, title));
            if let Some(model) = phase
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                out.push_str(&format!(" [model: {model}]"));
            }
            out.push('\n');
            if let Some(detail) = phase
                .get("detail")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                out.push_str("   ");
                out.push_str(detail);
                out.push('\n');
            }
        }
    }

    out.push_str("\nAgent calls\n");
    let calls = metadata
        .get("calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let agent_calls = calls
        .iter()
        .filter(|call| call.get("kind").and_then(Value::as_str) == Some("agent"))
        .collect::<Vec<_>>();
    if agent_calls.is_empty() {
        out.push_str("- (no agent calls found statically)\n");
    } else {
        for (idx, call) in agent_calls.iter().take(8).enumerate() {
            let line = call.get("line").and_then(Value::as_u64).unwrap_or(0);
            let summary = call
                .get("summary")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("(no summary)");
            out.push_str(&format!("{}. line {line}: {summary}\n", idx + 1));
        }
        if agent_calls.len() > 8 {
            out.push_str(&format!(
                "- ... {} more agent call(s) omitted\n",
                agent_calls.len() - 8
            ));
        }
    }

    out.push_str("\nWarnings/errors\n");
    let warnings = metadata
        .get("warnings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let errors = metadata
        .get("errors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if warnings.is_empty() && errors.is_empty() {
        out.push_str("- none\n");
    } else {
        for warning in warnings.iter().filter_map(Value::as_str) {
            out.push_str("- warning: ");
            out.push_str(warning.trim());
            out.push('\n');
        }
        for error in errors.iter().filter_map(Value::as_str) {
            out.push_str("- error: ");
            out.push_str(error.trim());
            out.push('\n');
        }
    }

    out.push_str("\nScript details\n");
    out.push_str("- JavaScript source is an implementation detail for audit. Review the plan, phases, and agent calls above first; inspect this excerpt only if needed.\n");
    if let Some(script_excerpt) = metadata
        .get("scriptExcerpt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        out.push_str("\nScript excerpt\n");
        out.push_str(script_excerpt);
        out.push('\n');
    } else {
        out.push_str("- No script excerpt available.\n");
    }

    Some(out)
}

pub fn build_workflow_revision_feedback(graph: &rebon_tools_core::WorkflowGraph) -> String {
    // The engine only treats this feedback as a revision request when it
    // starts with the shared prefix — keep the two sides single-sourced.
    let mut feedback = String::from(rebon_core::permission::WORKFLOW_REVISION_FEEDBACK_PREFIX);
    feedback.push_str(
        " Do not run the current workflow script. Regenerate a new Workflow call that incorporates these requested changes:\n",
    );
    for node in graph.modified_nodes() {
        feedback.push_str("\n- ");
        feedback.push_str(&node.title);
        feedback.push_str(" (");
        feedback.push_str(&node.id);
        feedback.push_str("): ");
        feedback.push_str(node.detail.as_deref().unwrap_or("(clear this requirement)"));
    }
    feedback
}
