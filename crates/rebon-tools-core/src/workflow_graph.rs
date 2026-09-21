use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGraph {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<WorkflowGraphNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<WorkflowGraphEdge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_text: Option<String>,
    /// Structural launch args (the Workflow tool's `args` input), so
    /// renderers can substitute `${args.…}` template placeholders with the
    /// actual values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGraphNode {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub node_type: WorkflowGraphNodeType,
    pub status: WorkflowGraphNodeStatus,
    pub editable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub modified: bool,
    /// Task id of the underlying sub-agent (`agent-<hex>`), when the node
    /// represents a live workflow agent whose spawned task is visible to the
    /// host. Lets a renderer link the node to the agent's execution view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Structured routing/config facts for `agent()` nodes (model, provider,
    /// isolation, …), populated when the source carried them. Renderers that
    /// only need the text projection can keep ignoring it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<WorkflowAgentNodeMeta>,
}

/// Per-`agent()` structured metadata extracted from the workflow script or
/// review preview. Every field is optional: a summary-only source (older
/// engine, dynamic expression) simply leaves the ones it cannot prove empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentNodeMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Prompt excerpt (statically visible part), for inspector display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Phase title the call names (option or surrounding `phase()`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_title: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_schema: bool,
    /// Identifier the `schema:` option names (e.g. `REVIEW_SCHEMA`), when it
    /// is a bare identifier rather than an inline object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_name: Option<String>,
}

impl WorkflowAgentNodeMeta {
    pub fn is_empty(&self) -> bool {
        self.label.is_none()
            && self.prompt.is_none()
            && self.model.is_none()
            && self.provider.is_none()
            && self.model_profile.is_none()
            && self.isolation.is_none()
            && self.agent_type.is_none()
            && self.phase_title.is_none()
            && !self.has_schema
            && self.schema_name.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum WorkflowGraphNodeType {
    Overview,
    Phase,
    Agent,
    Call,
    Warning,
    Error,
    Schema,
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum WorkflowGraphNodeStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: WorkflowGraphEdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum WorkflowGraphEdgeKind {
    Sequence,
    Contains,
    Pipeline,
    ParallelBarrier,
    Schema,
}

/// One phase while the progress stream is being replayed.
///
/// Lives at module scope rather than inside `from_progress_entries` so the
/// replay loop can be a named function that takes it.
#[derive(Debug)]
struct PhaseRecord {
    key: String,
    id_key: Option<String>,
    node_idx: usize,
    open: bool,
    implicit: bool,
}

/// One agent while the progress stream is being replayed.
#[derive(Debug)]
struct AgentRecord {
    phase_idx: usize,
    node_idx: usize,
    index: u64,
    label_key: String,
    order: usize,
}

/// Replay the ordered progress entries into the graph under construction.
///
/// Every accumulator is threaded through by reference because the passes that
/// follow -- phase edges, phase collapse, overview aggregation -- read the
/// same records this loop builds.
#[allow(clippy::too_many_arguments)]
fn replay_progress_entries(
    ordered: Vec<&Value>,
    graph: &mut WorkflowGraph,
    phases: &mut Vec<PhaseRecord>,
    active_phases: &mut Vec<usize>,
    agents: &mut Vec<AgentRecord>,
    agents_by_id: &mut HashMap<String, usize>,
    agents_by_index: &mut HashMap<(usize, u64), usize>,
    agents_by_label: &mut HashMap<(usize, String), usize>,
    global_agents_by_index: &mut HashMap<u64, Option<usize>>,
    logs: &mut Vec<String>,
) {
    for value in ordered {
        let Some(object) = value.as_object() else {
            continue;
        };
        let entry = object
            .get("entry")
            .and_then(Value::as_object)
            .unwrap_or(object);
        match entry.get("type").and_then(Value::as_str) {
            Some("phase") => apply_phase_entry(entry, graph, phases, active_phases),
            Some("agent") => apply_agent_entry(
                entry,
                graph,
                phases,
                active_phases,
                agents,
                agents_by_id,
                agents_by_index,
                agents_by_label,
                global_agents_by_index,
            ),
            Some("log") => {
                if let Some(message) = string_field(entry, "message") {
                    logs.push(message);
                }
            }
            _ => {}
        }
    }
}

impl WorkflowGraph {
    pub fn from_permission_metadata(metadata: &Value) -> Option<Self> {
        let metadata = metadata.as_object()?;
        if metadata
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "workflowReview")
        {
            return None;
        }

        let name = string_field(metadata, "name").unwrap_or_else(|| "workflow".to_string());
        let title = string_field(metadata, "title").unwrap_or_else(|| name.clone());
        let description = string_field(metadata, "description");
        let source = string_field(metadata, "source");
        let args_summary = string_field(metadata, "argsSummary");
        let agent_call_count = metadata
            .get("agentCallCount")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                call_array(metadata)
                    .filter(|call| call.kind == "agent")
                    .count() as u64
            });
        let total_call_count = metadata
            .get("totalCallCount")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| call_array(metadata).count() as u64);

        let mut graph = WorkflowGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            fallback_text: string_field(metadata, "reviewText"),
            args: metadata
                .get("args")
                .cloned()
                .filter(|value| !value.is_null()),
        };
        graph.nodes.push(WorkflowGraphNode {
            id: "overview".to_string(),
            title: format!("Workflow: {title}"),
            detail: Some(overview_detail(
                description.as_deref(),
                source.as_deref(),
                args_summary.as_deref(),
                agent_call_count,
                total_call_count,
            )),
            node_type: WorkflowGraphNodeType::Overview,
            status: WorkflowGraphNodeStatus::Pending,
            editable: true,
            modified: false,
            agent_id: None,
            agent: None,
        });

        let mut phase_ids = Vec::new();
        for (idx, phase) in phase_array(metadata).enumerate() {
            let title =
                string_field(phase, "title").unwrap_or_else(|| "(untitled phase)".to_string());
            let id = format!("phase:{idx}");
            let detail = phase_detail(
                string_field(phase, "detail").as_deref(),
                string_field(phase, "model").as_deref(),
            );
            graph.nodes.push(WorkflowGraphNode {
                id: id.clone(),
                title: format!("Phase {}: {title}", idx + 1),
                detail,
                node_type: WorkflowGraphNodeType::Phase,
                status: WorkflowGraphNodeStatus::Pending,
                editable: true,
                modified: false,
                agent_id: None,
                agent: None,
            });
            phase_ids.push((normalized_key(&title), id));
        }
        for idx in 0..phase_ids.len() {
            let from = if idx == 0 {
                "overview".to_string()
            } else {
                phase_ids[idx - 1].1.clone()
            };
            graph.edges.push(WorkflowGraphEdge {
                from,
                to: phase_ids[idx].1.clone(),
                kind: WorkflowGraphEdgeKind::Sequence,
            });
        }

        let mut current_phase_id = phase_ids.first().map(|(_, id)| id.clone());
        let mut last_sequence_id = phase_ids
            .last()
            .map(|(_, id)| id.clone())
            .unwrap_or_else(|| "overview".to_string());
        let mut agent_no = 0usize;
        for (idx, call) in call_array(metadata).enumerate() {
            let kind = call.kind.as_str();
            if kind == "phase" {
                let phase_title = phase_title_from_call_summary(&call.summary);
                if let Some(existing) = phase_title
                    .as_deref()
                    .and_then(|title| find_phase_id(&phase_ids, title))
                {
                    current_phase_id = Some(existing.to_string());
                    last_sequence_id = existing.to_string();
                    continue;
                }
                let title = phase_title.unwrap_or_else(|| call.summary.clone());
                let id = format!("phase-call:{idx}:line:{}", call.line);
                graph.nodes.push(WorkflowGraphNode {
                    id: id.clone(),
                    title: format!("Phase: {title}"),
                    detail: Some(format!("line {}: {}", call.line, call.summary)),
                    node_type: WorkflowGraphNodeType::Phase,
                    status: WorkflowGraphNodeStatus::Pending,
                    editable: true,
                    modified: false,
                    agent_id: None,
                    agent: None,
                });
                graph.edges.push(WorkflowGraphEdge {
                    from: last_sequence_id.clone(),
                    to: id.clone(),
                    kind: WorkflowGraphEdgeKind::Sequence,
                });
                current_phase_id = Some(id.clone());
                last_sequence_id = id;
                continue;
            }

            let node_id = format!("call:{idx}:line:{}:{kind}", call.line);
            let node_type = if kind == "agent" {
                WorkflowGraphNodeType::Agent
            } else {
                WorkflowGraphNodeType::Call
            };
            let meta = call.agent_meta();
            // Agent nodes carry an editable *name* and *prompt*, so they get
            // the parsed fields, never the raw `line N: prompt "…" (…)`
            // summary — that string leaking into the editors is what the
            // inspector then asks the user to "edit".
            let (title, detail) = if kind == "agent" {
                agent_no += 1;
                let name = meta
                    .as_ref()
                    .and_then(|meta| meta.label.clone())
                    .map(|label| label.trim().to_string())
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| format!("Agent {agent_no}"));
                // Parsed prompt first; a summary that never carried a
                // `prompt "…"` shape (older engines) still shows as-is
                // rather than vanishing. A prompt that is itself a script
                // expression (`lens.prompt` in a mapped call) is NOT prompt
                // text — leave the detail empty and let renderers surface it
                // as a dynamic marker via the structured meta instead.
                let prompt = meta
                    .as_ref()
                    .and_then(|meta| meta.prompt.clone())
                    .map(|prompt| prompt.trim().to_string())
                    .filter(|prompt| !prompt.is_empty());
                let detail = match prompt {
                    Some(prompt) if workflow_prompt_is_dynamic_expression(&prompt) => None,
                    Some(prompt) => Some(prompt),
                    None => {
                        let summary = call.summary.trim();
                        (!summary.is_empty()).then(|| summary.to_string())
                    }
                };
                (name, detail)
            } else {
                (
                    call_title(kind, idx, call.line, &call.summary),
                    call_detail(
                        kind,
                        call.line,
                        &call.summary,
                        call.phase.as_deref(),
                        call.has_schema,
                    ),
                )
            };
            graph.nodes.push(WorkflowGraphNode {
                id: node_id.clone(),
                title,
                detail,
                node_type,
                status: WorkflowGraphNodeStatus::Pending,
                editable: true,
                modified: false,
                agent_id: None,
                agent: meta,
            });

            let explicit_phase = call
                .phase
                .clone()
                .or_else(|| summary_option(&call.summary, "phase"));
            let parent = explicit_phase
                .as_deref()
                .and_then(|phase| find_phase_id(&phase_ids, phase).map(str::to_string))
                .or_else(|| current_phase_id.clone())
                .unwrap_or_else(|| "overview".to_string());
            graph.edges.push(WorkflowGraphEdge {
                from: parent,
                to: node_id.clone(),
                kind: edge_kind_for_call(kind),
            });
            last_sequence_id = node_id.clone();

            if kind == "agent" && call.has_schema {
                let schema_id = format!("schema:{idx}:line:{}", call.line);
                graph.nodes.push(WorkflowGraphNode {
                    id: schema_id.clone(),
                    title: "Schema contract".to_string(),
                    detail: Some(
                        "Structured output schema is display-only here; revise the agent requirements without weakening the schema contract."
                            .to_string(),
                    ),
                    node_type: WorkflowGraphNodeType::Schema,
                    status: WorkflowGraphNodeStatus::Info,
                    editable: false,
                    modified: false,
                    agent_id: None,
                    agent: None,
                });
                graph.edges.push(WorkflowGraphEdge {
                    from: node_id,
                    to: schema_id,
                    kind: WorkflowGraphEdgeKind::Schema,
                });
            }
        }

        for (idx, warning) in string_array(metadata, "warnings").enumerate() {
            let id = format!("warning:{idx}");
            graph.nodes.push(WorkflowGraphNode {
                id: id.clone(),
                title: "Warning".to_string(),
                detail: Some(warning),
                node_type: WorkflowGraphNodeType::Warning,
                status: WorkflowGraphNodeStatus::Warning,
                editable: false,
                modified: false,
                agent_id: None,
                agent: None,
            });
            graph.edges.push(WorkflowGraphEdge {
                from: "overview".to_string(),
                to: id,
                kind: WorkflowGraphEdgeKind::Contains,
            });
        }
        for (idx, error) in string_array(metadata, "errors").enumerate() {
            let id = format!("error:{idx}");
            graph.nodes.push(WorkflowGraphNode {
                id: id.clone(),
                title: "Error".to_string(),
                detail: Some(error),
                node_type: WorkflowGraphNodeType::Error,
                status: WorkflowGraphNodeStatus::Failed,
                editable: false,
                modified: false,
                agent_id: None,
                agent: None,
            });
            graph.edges.push(WorkflowGraphEdge {
                from: "overview".to_string(),
                to: id,
                kind: WorkflowGraphEdgeKind::Contains,
            });
        }

        graph.bound_display_fields();
        Some(graph)
    }

    pub fn from_progress(progress: &Value) -> Option<Self> {
        let entries = progress.get("entries")?.as_array()?;
        Some(Self::from_progress_entries(entries, progress))
    }

    /// Builds a runtime graph from the persisted `workflowProgress.entries`
    /// stream. Wrapped entries are replayed by `sequence`; bare legacy entries
    /// retain input order.
    pub fn from_progress_entries(entries: &[Value], meta: &Value) -> Self {
        let name = meta
            .get("workflowName")
            .and_then(Value::as_str)
            .unwrap_or("workflow")
            .to_string();
        let summary = meta.get("summary").and_then(Value::as_str);
        let mut graph = WorkflowGraph {
            nodes: vec![WorkflowGraphNode {
                id: "overview".to_string(),
                title: format!("Workflow: {name}"),
                detail: summary.map(str::to_string),
                node_type: WorkflowGraphNodeType::Overview,
                status: WorkflowGraphNodeStatus::Pending,
                editable: false,
                modified: false,
                agent_id: None,
                agent: None,
            }],
            edges: Vec::new(),
            fallback_text: None,
            args: meta.get("args").cloned().filter(|value| !value.is_null()),
        };

        let mut ordered: Vec<&Value> = entries.iter().collect();
        if ordered
            .iter()
            .all(|value| value.get("sequence").and_then(Value::as_u64).is_some())
        {
            ordered.sort_by_key(|value| {
                value
                    .get("sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
            });
        }

        let mut phases: Vec<PhaseRecord> = Vec::new();
        let mut active_phases: Vec<usize> = Vec::new();
        let mut agents: Vec<AgentRecord> = Vec::new();
        let mut agents_by_id: HashMap<String, usize> = HashMap::new();
        let mut agents_by_index: HashMap<(usize, u64), usize> = HashMap::new();
        let mut agents_by_label: HashMap<(usize, String), usize> = HashMap::new();
        let mut global_agents_by_index: HashMap<u64, Option<usize>> = HashMap::new();
        let mut logs = Vec::new();

        replay_progress_entries(
            ordered,
            &mut graph,
            &mut phases,
            &mut active_phases,
            &mut agents,
            &mut agents_by_id,
            &mut agents_by_index,
            &mut agents_by_label,
            &mut global_agents_by_index,
            &mut logs,
        );

        let mut last = "overview".to_string();
        for phase in &phases {
            let id = graph.nodes[phase.node_idx].id.clone();
            graph.edges.push(WorkflowGraphEdge {
                from: last,
                to: id.clone(),
                kind: WorkflowGraphEdgeKind::Sequence,
            });
            last = id;
        }

        for (phase_idx, phase) in phases.iter().enumerate() {
            let mut phase_agents: Vec<&AgentRecord> = agents
                .iter()
                .filter(|agent| agent.phase_idx == phase_idx)
                .collect();
            phase_agents.sort_by_key(|agent| {
                (
                    if agent.index == 0 {
                        u64::MAX
                    } else {
                        agent.index
                    },
                    agent.order,
                )
            });
            for agent in phase_agents {
                graph.edges.push(WorkflowGraphEdge {
                    from: graph.nodes[phase.node_idx].id.clone(),
                    to: graph.nodes[agent.node_idx].id.clone(),
                    kind: WorkflowGraphEdgeKind::Contains,
                });
            }
        }

        for (phase_idx, phase) in phases.iter().enumerate() {
            if !phase.implicit {
                continue;
            }
            let status = aggregate_runtime_status(
                agents
                    .iter()
                    .filter(|agent| agent.phase_idx == phase_idx)
                    .map(|agent| graph.nodes[agent.node_idx].status),
            )
            .unwrap_or(WorkflowGraphNodeStatus::Info);
            graph.nodes[phase.node_idx].status = status;
        }

        graph.fallback_text = (!logs.is_empty()).then(|| {
            logs.into_iter()
                .map(|log| format!("Log: {log}"))
                .collect::<Vec<_>>()
                .join("\n")
        });
        // Pipelines interleave phases, and a marker-style phase never gets an
        // explicit "completed" entry — collapse a running phase once every
        // agent it contains has turned terminal, so the canvas doesn't show
        // the whole workflow as simultaneously live. The collapse is
        // failure-aware: a phase over a failed agent turns Failed, not
        // Completed — a green phase above a red agent misreports the run.
        let statuses: HashMap<&str, WorkflowGraphNodeStatus> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .map(|node| (node.id.as_str(), node.status))
            .collect();
        let mut phase_members: HashMap<&str, Vec<WorkflowGraphNodeStatus>> = HashMap::new();
        for edge in &graph.edges {
            if edge.kind == WorkflowGraphEdgeKind::Contains {
                if let Some(&status) = statuses.get(edge.to.as_str()) {
                    phase_members
                        .entry(edge.from.as_str())
                        .or_default()
                        .push(status);
                }
            }
        }
        let settled: Vec<(String, WorkflowGraphNodeStatus)> = graph
            .nodes
            .iter()
            .filter(|node| {
                node.node_type == WorkflowGraphNodeType::Phase
                    && node.status == WorkflowGraphNodeStatus::Running
            })
            .filter_map(|node| {
                let members = phase_members.get(node.id.as_str())?;
                let all_terminal = !members.is_empty()
                    && members.iter().all(|status| {
                        matches!(
                            status,
                            WorkflowGraphNodeStatus::Completed | WorkflowGraphNodeStatus::Failed
                        )
                    });
                if !all_terminal {
                    return None;
                }
                let status = if members.contains(&WorkflowGraphNodeStatus::Failed) {
                    WorkflowGraphNodeStatus::Failed
                } else {
                    WorkflowGraphNodeStatus::Completed
                };
                Some((node.id.clone(), status))
            })
            .collect();
        for node in &mut graph.nodes {
            if let Some((_, status)) = settled.iter().find(|(id, _)| id == &node.id) {
                node.status = *status;
            }
        }
        // The overview aggregates AFTER the phase collapse: computed before
        // it, the root stayed Running forever once every agent had settled
        // but the marker phases had not been rewritten yet.
        graph.nodes[0].status = aggregate_runtime_status(
            graph
                .nodes
                .iter()
                .skip(1)
                .filter(|node| {
                    matches!(
                        node.node_type,
                        WorkflowGraphNodeType::Phase | WorkflowGraphNodeType::Agent
                    )
                })
                .map(|node| node.status),
        )
        .unwrap_or(WorkflowGraphNodeStatus::Pending);
        graph.bound_display_fields();
        graph
    }

    fn bound_display_fields(&mut self) {
        for node in &mut self.nodes {
            node.title = bounded_graph_text(&node.title, 512);
            node.detail = node
                .detail
                .as_deref()
                .map(|detail| bounded_graph_text(detail, 2_048));
            node.agent_id = node
                .agent_id
                .as_deref()
                .map(|agent_id| bounded_graph_text(agent_id, 256));
            if let Some(meta) = node.agent.as_mut() {
                let bound = |value: &mut Option<String>, max: usize| {
                    *value = value.as_deref().map(|text| bounded_graph_text(text, max));
                };
                bound(&mut meta.label, 256);
                bound(&mut meta.prompt, 2_048);
                bound(&mut meta.model, 256);
                bound(&mut meta.provider, 256);
                bound(&mut meta.model_profile, 256);
                bound(&mut meta.isolation, 128);
                bound(&mut meta.agent_type, 256);
                bound(&mut meta.phase_title, 512);
                bound(&mut meta.schema_name, 256);
            }
        }
        self.fallback_text = self
            .fallback_text
            .as_deref()
            .map(|text| bounded_graph_text(text, 8_192));
    }

    pub fn has_modified_nodes(&self) -> bool {
        self.nodes.iter().any(|node| node.modified)
    }

    pub fn modified_nodes(&self) -> impl Iterator<Item = &WorkflowGraphNode> {
        self.nodes.iter().filter(|node| node.modified)
    }

    pub fn focused_node(&self, focused_index: usize) -> Option<&WorkflowGraphNode> {
        self.nodes.get(focused_index)
    }

    pub fn focused_node_mut(&mut self, focused_index: usize) -> Option<&mut WorkflowGraphNode> {
        self.nodes.get_mut(focused_index)
    }

    pub fn terminal_lines(
        &self,
        focused_index: Option<usize>,
        include_details: bool,
    ) -> Vec<String> {
        if self.nodes.is_empty() {
            return self
                .fallback_text
                .as_deref()
                .map(|text| text.lines().map(str::to_string).collect())
                .unwrap_or_default();
        }

        let mut lines = Vec::new();
        for (idx, node) in self.nodes.iter().enumerate() {
            let focused = focused_index == Some(idx);
            let marker = if focused { "›" } else { " " };
            let modified = if node.modified { " *" } else { "" };
            let edit = if node.editable { "" } else { " (read-only)" };
            lines.push(format!(
                "{marker} {} {}{}{}",
                status_label(node.status),
                typed_title(node),
                modified,
                edit
            ));
            if include_details || focused {
                if let Some(detail) = node
                    .detail
                    .as_deref()
                    .filter(|detail| !detail.trim().is_empty())
                {
                    for detail_line in detail.lines().take(8) {
                        lines.push(format!("    {detail_line}"));
                    }
                }
            }
        }
        lines
    }
}

fn aggregate_runtime_status(
    statuses: impl IntoIterator<Item = WorkflowGraphNodeStatus>,
) -> Option<WorkflowGraphNodeStatus> {
    let mut saw_status = false;
    let mut saw_failed = false;
    let mut saw_running = false;
    let mut saw_warning = false;
    let mut saw_pending = false;
    let mut saw_completed = false;
    let mut saw_info = false;
    for status in statuses {
        saw_status = true;
        match status {
            WorkflowGraphNodeStatus::Failed => saw_failed = true,
            WorkflowGraphNodeStatus::Running => saw_running = true,
            WorkflowGraphNodeStatus::Warning => saw_warning = true,
            WorkflowGraphNodeStatus::Pending => saw_pending = true,
            WorkflowGraphNodeStatus::Completed => saw_completed = true,
            WorkflowGraphNodeStatus::Info => saw_info = true,
        }
    }
    if !saw_status {
        None
    } else if saw_failed {
        Some(WorkflowGraphNodeStatus::Failed)
    } else if saw_running {
        Some(WorkflowGraphNodeStatus::Running)
    } else if saw_warning {
        Some(WorkflowGraphNodeStatus::Warning)
    } else if saw_pending {
        Some(WorkflowGraphNodeStatus::Pending)
    } else if saw_completed {
        Some(WorkflowGraphNodeStatus::Completed)
    } else if saw_info {
        Some(WorkflowGraphNodeStatus::Info)
    } else {
        None
    }
}

impl WorkflowGraphNodeStatus {
    pub fn from_runtime_state(state: &str) -> Self {
        match state {
            "completed" | "done" => Self::Completed,
            "error" | "failed" => Self::Failed,
            "start" | "started" | "running" | "pending" => Self::Running,
            _ => Self::Info,
        }
    }
}

struct PermissionCall {
    kind: String,
    line: u64,
    summary: String,
    phase: Option<String>,
    has_schema: bool,
    agent: Option<WorkflowAgentNodeMeta>,
}

impl PermissionCall {
    /// Structured agent metadata: prefer the explicit `agent` object newer
    /// engines attach to each call; fall back to parsing the display summary
    /// (`prompt "…" (model=…, provider=…)`) an older engine produced.
    fn agent_meta(&self) -> Option<WorkflowAgentNodeMeta> {
        if self.kind != "agent" {
            return None;
        }
        let mut meta = self.agent.clone().unwrap_or_default();
        if meta.model.is_none() {
            meta.model = summary_option(&self.summary, "model");
        }
        if meta.provider.is_none() {
            meta.provider = summary_option(&self.summary, "provider");
        }
        if meta.agent_type.is_none() {
            meta.agent_type = summary_option(&self.summary, "agentType");
        }
        if meta.label.is_none() {
            meta.label = summary_option(&self.summary, "label");
        }
        if meta.prompt.is_none() {
            meta.prompt = prompt_from_summary(&self.summary);
        }
        if meta.phase_title.is_none() {
            meta.phase_title = self
                .phase
                .clone()
                .or_else(|| summary_option(&self.summary, "phase"));
        }
        meta.has_schema = meta.has_schema || self.has_schema;
        (!meta.is_empty()).then_some(meta)
    }
}

fn overview_detail(
    description: Option<&str>,
    source: Option<&str>,
    args_summary: Option<&str>,
    agent_call_count: u64,
    total_call_count: u64,
) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "Requirements: {}",
        description.unwrap_or("(missing; ask the model to clarify the workflow goal)")
    ));
    if let Some(source) = source {
        parts.push(format!("Source: {source}"));
    }
    parts.push(format!("Args: {}", args_summary.unwrap_or("(none)")));
    parts.push(format!(
        "Static calls: {agent_call_count} agent call(s), {total_call_count} total call(s)"
    ));
    parts.join("\n")
}

fn phase_detail(detail: Option<&str>, model: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(detail) = detail.filter(|detail| !detail.trim().is_empty()) {
        parts.push(format!("Requirements: {}", detail.trim()));
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        parts.push(format!("Model: {}", model.trim()));
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// A "prompt" that is actually an unevaluated script expression —
/// `lens.prompt`, `item[0].q`, `promptFor(lens)` in a mapped `agent()` call —
/// rather than prompt text: one code-shaped token with no natural-language
/// whitespace. Templates that mix text with `${…}` interpolations are NOT
/// dynamic — the template itself is the best static description of the
/// prompt.
pub fn workflow_prompt_is_dynamic_expression(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty()
        && text.chars().any(|ch| matches!(ch, '.' | '(' | '[' | '$'))
        // Every character must be code-shaped — one natural-language char
        // (including CJK text, which has no spaces to test for) makes it a
        // prompt, not an expression.
        && text.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '.' | '(' | ')' | '[' | ']' | '$' | '{' | '}' | '_' | ',' | ':' | '-' | '\''
                        | '"' | '`'
                )
        })
}

/// Byte ranges of `${…}` interpolations (nested braces balanced). `$`/`{`/`}`
/// are ASCII, so every range edge is a valid UTF-8 boundary even in CJK text.
pub fn workflow_template_placeholder_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'{' {
            let mut depth = 0usize;
            let mut j = i + 1;
            let mut end = None;
            while j < bytes.len() {
                match bytes[j] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(j + 1);
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            let Some(end) = end else { break };
            ranges.push(i..end);
            i = end;
            continue;
        }
        i += 1;
    }
    ranges
}

/// Resolve a `${args.…}` interpolation against the workflow's launch args:
/// `args`, `args.topic`, `args.items[0].name`, `args["key"]`. Anything with
/// operators or unknown roots stays unresolved (shown as a placeholder).
pub fn resolve_workflow_args_path(args: &Value, expr: &str) -> Option<String> {
    let expr = expr.trim();
    let mut rest = expr.strip_prefix("args")?;
    let mut current = args;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let end = after
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'))
                .unwrap_or(after.len());
            if end == 0 {
                return None;
            }
            current = current.get(&after[..end])?;
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let close = after.find(']')?;
            let index = after[..close].trim();
            if let Ok(n) = index.parse::<usize>() {
                current = current.get(n)?;
            } else {
                let key = index.trim_matches(|ch| matches!(ch, '\'' | '"' | '`'));
                if key.len() == index.len() {
                    return None;
                }
                current = current.get(key)?;
            }
            rest = &after[close + 1..];
        } else {
            return None;
        }
    }
    Some(match current {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "null".to_string(),
        other => {
            let raw = serde_json::to_string(other).ok()?;
            if raw.chars().count() > 120 {
                let mut clipped: String = raw.chars().take(120).collect();
                clipped.push('…');
                clipped
            } else {
                raw
            }
        }
    })
}

/// Result of expanding a workflow template against launch args. Renderers
/// deciding whether a prompt is "still dynamic" must consult the counters,
/// not the lexical shape of `text`: a resolved value may look like an
/// expression (`foo.bar`) and an unresolved remainder may hide inside
/// natural-language text.
pub struct ExpandedWorkflowTemplate {
    pub text: String,
    /// `${…}` placeholders whose path resolved to a launch-arg value.
    pub resolved_placeholders: usize,
    /// `${…}` placeholders left verbatim (computations, unknown roots, or
    /// no args available).
    pub unresolved_placeholders: usize,
}

/// Replace every resolvable `${args.…}` placeholder in `text` with its
/// launch-arg value. Computations and unknown roots stay verbatim, so the
/// reader still sees that a dynamic piece exists there.
pub fn expand_workflow_template(text: &str, args: Option<&Value>) -> ExpandedWorkflowTemplate {
    let mut out = String::new();
    let mut resolved = 0usize;
    let mut unresolved = 0usize;
    let mut last = 0usize;
    for range in workflow_template_placeholder_ranges(text) {
        out.push_str(&text[last..range.start]);
        let inner = &text[range.start + 2..range.end - 1];
        match args.and_then(|args| resolve_workflow_args_path(args, inner)) {
            Some(value) => {
                resolved += 1;
                out.push_str(&value);
            }
            None => {
                unresolved += 1;
                out.push_str(&text[range.clone()]);
            }
        }
        last = range.end;
    }
    out.push_str(&text[last..]);
    ExpandedWorkflowTemplate {
        text: out,
        resolved_placeholders: resolved,
        unresolved_placeholders: unresolved,
    }
}

/// [`expand_workflow_template`] when only the substituted text matters.
pub fn expand_workflow_template_text(text: &str, args: Option<&Value>) -> String {
    expand_workflow_template(text, args).text
}

fn call_detail(
    kind: &str,
    line: u64,
    summary: &str,
    phase: Option<&str>,
    has_schema: bool,
) -> Option<String> {
    let mut parts = vec![format!("line {line}: {summary}")];
    if let Some(phase) = phase.filter(|phase| !phase.trim().is_empty()) {
        parts.push(format!("Phase: {}", phase.trim()));
    }
    match kind {
        "pipeline" => parts.push(
            "Pipeline is non-barrier: each item flows independently through stages; failed items become null."
                .to_string(),
        ),
        "parallel" => parts.push(
            "Parallel is a barrier: downstream steps wait for all branches and failed branches resolve to null."
                .to_string(),
        ),
        "agent" if has_schema => parts.push(
            "Agent returns structured output through a schema; edit requirements without weakening that contract."
                .to_string(),
        ),
        _ => {}
    }
    Some(parts.join("\n"))
}

fn call_title(kind: &str, idx: usize, line: u64, summary: &str) -> String {
    match kind {
        "agent" => format!("Agent {}: {}", idx + 1, summary),
        "pipeline" => format!("Pipeline: {summary}"),
        "parallel" => format!("Parallel barrier: {summary}"),
        "log" => format!("Log: {summary}"),
        _ => format!("{} line {line}: {summary}", title_case(kind)),
    }
}

fn edge_kind_for_call(kind: &str) -> WorkflowGraphEdgeKind {
    match kind {
        "pipeline" => WorkflowGraphEdgeKind::Pipeline,
        "parallel" => WorkflowGraphEdgeKind::ParallelBarrier,
        _ => WorkflowGraphEdgeKind::Contains,
    }
}

pub fn status_label(status: WorkflowGraphNodeStatus) -> &'static str {
    match status {
        WorkflowGraphNodeStatus::Pending => "[pending]",
        WorkflowGraphNodeStatus::Running => "[running]",
        WorkflowGraphNodeStatus::Completed => "[done]",
        WorkflowGraphNodeStatus::Failed => "[failed]",
        WorkflowGraphNodeStatus::Warning => "[warning]",
        WorkflowGraphNodeStatus::Info => "[info]",
    }
}

fn typed_title(node: &WorkflowGraphNode) -> String {
    let kind = match node.node_type {
        WorkflowGraphNodeType::Overview => "Overview",
        WorkflowGraphNodeType::Phase => "Phase",
        WorkflowGraphNodeType::Agent => "Agent",
        WorkflowGraphNodeType::Call => "Call",
        WorkflowGraphNodeType::Warning => "Warning",
        WorkflowGraphNodeType::Error => "Error",
        WorkflowGraphNodeType::Schema => "Schema",
        WorkflowGraphNodeType::Log => "Log",
    };
    format!("{kind}: {}", node.title)
}

fn phase_array(
    metadata: &serde_json::Map<String, Value>,
) -> impl Iterator<Item = &serde_json::Map<String, Value>> {
    metadata
        .get("phases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

fn call_array(
    metadata: &serde_json::Map<String, Value>,
) -> impl Iterator<Item = PermissionCall> + '_ {
    metadata
        .get("calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let call = call.as_object()?;
            Some(PermissionCall {
                kind: string_field(call, "kind")?,
                line: call.get("line").and_then(Value::as_u64).unwrap_or(0),
                summary: string_field(call, "summary")
                    .unwrap_or_else(|| "(no summary)".to_string()),
                phase: string_field(call, "phase"),
                has_schema: call
                    .get("hasSchema")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| {
                        call.get("summary")
                            .and_then(Value::as_str)
                            .is_some_and(summary_has_schema)
                    }),
                agent: call
                    .get("agent")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok()),
            })
        })
}

fn string_array<'a>(
    metadata: &'a serde_json::Map<String, Value>,
    key: &'static str,
) -> impl Iterator<Item = String> + 'a {
    metadata
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalized_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn find_phase_id<'a>(phase_ids: &'a [(String, String)], phase: &str) -> Option<&'a str> {
    let key = normalized_key(phase);
    phase_ids
        .iter()
        .find(|(title, _)| *title == key)
        .map(|(_, id)| id.as_str())
}

fn phase_title_from_call_summary(summary: &str) -> Option<String> {
    let title = summary.trim().strip_prefix("phase")?.trim();
    clean_option_value(title).filter(|value| !value.is_empty())
}

fn summary_option(summary: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = summary.find(&needle)? + needle.len();
    let rest = &summary[start..];
    let end = rest.find([',', ')', '\n']).unwrap_or(rest.len());
    clean_option_value(&rest[..end]).filter(|value| !value.is_empty())
}

fn clean_option_value(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string();
    (!value.is_empty()).then_some(value)
}

/// Pull the statically-scanned prompt excerpt out of an agent call summary of
/// the form `prompt "…" (opts)` / `prompt `…``.
fn prompt_from_summary(summary: &str) -> Option<String> {
    let rest = summary.trim_start().strip_prefix("prompt ")?.trim_start();
    let quote = rest
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '`' | '\''))?;
    let body = &rest[quote.len_utf8()..];
    let end = body.find(quote)?;
    let value = body[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn summary_has_schema(summary: &str) -> bool {
    summary.contains("schema=")
        || summary.contains("schema: ")
        || summary.contains("schema present")
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Call".to_string(),
    }
}

fn bounded_graph_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let text: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{text}…")
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_metadata_to_graph_derives_nodes_edges_and_semantics() {
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "title": "Demo Workflow",
            "description": "Review files and verify changes",
            "source": "inline",
            "argsSummary": "{\"scope\":\"repo\"}",
            "phases": [
                {"title":"Inspect", "detail":"Find files"},
                {"title":"Verify", "detail":"Run checks", "model":"m1"}
            ],
            "calls": [
                {"kind":"phase", "line":3, "summary":"phase \"Inspect\""},
                {"kind":"agent", "line":4, "summary":"prompt \"inspect\" (phase=Inspect, schema=present)"},
                {"kind":"pipeline", "line":8, "summary":"pipeline with 2 step(s)"},
                {"kind":"parallel", "line":12, "summary":"parallel with 3 branch(es)"}
            ],
            "warnings": ["fallback used"],
            "errors": ["bad meta"]
        });

        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");

        assert_eq!(graph.nodes[0].id, "overview");
        assert_eq!(graph.nodes[0].node_type, WorkflowGraphNodeType::Overview);
        assert!(graph.nodes.iter().any(|node| node.id == "phase:0"));
        assert!(graph.nodes.iter().any(|node| node.id == "phase:1"));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.node_type == WorkflowGraphNodeType::Agent));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.node_type == WorkflowGraphNodeType::Schema && !node.editable));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.node_type == WorkflowGraphNodeType::Warning));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.node_type == WorkflowGraphNodeType::Error));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:0"
                && edge.to.starts_with("call:1")
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.kind == WorkflowGraphEdgeKind::Pipeline));
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.kind == WorkflowGraphEdgeKind::ParallelBarrier));
        assert!(graph
            .terminal_lines(Some(1), false)
            .iter()
            .any(|line| line.contains("› [pending] Phase")));
    }

    #[test]
    fn permission_metadata_structured_agent_object_feeds_node_meta() {
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "phases": [{"title":"Run"}],
            "calls": [
                {
                    "kind":"agent",
                    "line":4,
                    "summary":"prompt \"inspect\" (model=m1, phase=Run)",
                    "phase":"Run",
                    "hasSchema": true,
                    "agent": {
                        "label": "inspector",
                        "prompt": "inspect the repo",
                        "model": "m1",
                        "provider": "p1",
                        "modelProfile": "high",
                        "isolation": "worktree",
                        "agentType": "code-reviewer",
                        "schemaName": "REVIEW_SCHEMA"
                    }
                }
            ]
        });

        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        let agent = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .expect("agent node");
        let meta = agent.agent.as_ref().expect("agent meta");
        assert_eq!(meta.label.as_deref(), Some("inspector"));
        assert_eq!(meta.prompt.as_deref(), Some("inspect the repo"));
        assert_eq!(meta.model.as_deref(), Some("m1"));
        assert_eq!(meta.provider.as_deref(), Some("p1"));
        assert_eq!(meta.model_profile.as_deref(), Some("high"));
        assert_eq!(meta.isolation.as_deref(), Some("worktree"));
        assert_eq!(meta.agent_type.as_deref(), Some("code-reviewer"));
        assert_eq!(meta.phase_title.as_deref(), Some("Run"));
        assert!(meta.has_schema);
        assert_eq!(meta.schema_name.as_deref(), Some("REVIEW_SCHEMA"));
    }

    /// A mapped call's prompt is a script expression, not prompt text — the
    /// node keeps the template name but must not present `lens.prompt` as if
    /// it were the agent's actual prompt.
    #[test]
    fn permission_metadata_dynamic_prompt_expression_stays_out_of_the_detail() {
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "phases": [{"title":"Verify"}],
            "calls": [
                {
                    "kind":"agent",
                    "line":9,
                    "summary":"prompt lens.prompt (label=`verify:${lens.key}`, phase=Verify)",
                    "phase":"Verify",
                    "hasSchema": false,
                    "agent": {
                        "label": "verify:${lens.key}",
                        "prompt": "lens.prompt"
                    }
                }
            ]
        });

        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        let agent = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .expect("agent node");
        assert_eq!(agent.title, "verify:${lens.key}");
        assert_eq!(
            agent.detail, None,
            "expression must not pose as prompt text"
        );
        let meta = agent.agent.as_ref().expect("meta");
        assert_eq!(meta.prompt.as_deref(), Some("lens.prompt"));
        assert!(workflow_prompt_is_dynamic_expression("lens.prompt"));
        assert!(workflow_prompt_is_dynamic_expression("json(candidate)"));
        assert!(!workflow_prompt_is_dynamic_expression(
            "你是严格的反向验证者。检查角度：${lens.key}"
        ));
    }

    #[test]
    fn permission_metadata_without_agent_object_parses_summary_fallback() {
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "phases": [{"title":"Run"}],
            "calls": [
                {
                    "kind":"agent",
                    "line":4,
                    "summary":"prompt \"inspect the diff\" (model=m2, provider=p2, phase=Run, schema=present)"
                }
            ]
        });

        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        let agent = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .expect("agent node");
        let meta = agent.agent.as_ref().expect("agent meta from summary");
        assert_eq!(meta.model.as_deref(), Some("m2"));
        assert_eq!(meta.provider.as_deref(), Some("p2"));
        assert_eq!(meta.prompt.as_deref(), Some("inspect the diff"));
        assert_eq!(meta.phase_title.as_deref(), Some("Run"));
        assert!(meta.has_schema);
        // Non-agent calls never grow agent meta.
        assert!(graph
            .nodes
            .iter()
            .filter(|node| node.node_type != WorkflowGraphNodeType::Agent)
            .all(|node| node.agent.is_none()));
    }

    #[test]
    fn graph_tracks_modified_nodes() {
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "phases": [{"title":"Run", "detail":"Do work"}],
            "calls": []
        });
        let mut graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        assert!(!graph.has_modified_nodes());
        let node = graph.focused_node_mut(1).expect("phase node");
        node.detail = Some("Requirements: Do safer work".into());
        node.modified = true;
        assert!(graph.has_modified_nodes());
        assert_eq!(graph.modified_nodes().count(), 1);
    }

    #[test]
    fn permission_metadata_adds_unmatched_phase_calls_with_existing_meta_phases() {
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "phases": [{"title":"Inspect"}],
            "calls": [
                {"kind":"phase", "line":10, "summary":"phase \"Dynamic\""},
                {"kind":"agent", "line":11, "summary":"prompt \"dynamic\""}
            ]
        });

        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        let synthetic_phase = graph
            .nodes
            .iter()
            .find(|node| node.id.starts_with("phase-call:0") && node.title.contains("Dynamic"))
            .expect("synthetic unmatched phase node");
        assert!(graph.edges.iter().any(|edge| {
            edge.from == synthetic_phase.id
                && edge.to.starts_with("call:1")
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));
    }

    #[test]
    fn permission_metadata_uses_structured_agent_phase_and_schema() {
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "phases": [{"title":"Inspect"}, {"title":"Verify"}],
            "calls": [
                {
                    "kind":"agent",
                    "line":12,
                    "summary":"prompt \"verify\"",
                    "phase":"Verify",
                    "hasSchema": true
                }
            ]
        });

        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:1"
                && edge.to.starts_with("call:0")
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.node_type == WorkflowGraphNodeType::Schema));
    }

    /// The expansion counters — not the lexical shape of the output — say
    /// whether a template is still dynamic: a resolved value may look like
    /// an expression, and an unresolved remainder may hide inside CJK text.
    #[test]
    fn expand_workflow_template_counts_placeholder_resolution() {
        let args = json!({"topic": "缓存 策略", "path": "foo.bar"});
        let partial = expand_workflow_template("${args.topic}-${lens.key}", Some(&args));
        assert_eq!(partial.text, "缓存 策略-${lens.key}");
        assert_eq!(partial.resolved_placeholders, 1);
        assert_eq!(partial.unresolved_placeholders, 1);

        let full = expand_workflow_template("${args.path}", Some(&args));
        assert_eq!(full.text, "foo.bar");
        assert_eq!(full.resolved_placeholders, 1);
        assert_eq!(full.unresolved_placeholders, 0);

        let no_args = expand_workflow_template("${args.path}", None);
        assert_eq!(no_args.text, "${args.path}");
        assert_eq!(no_args.unresolved_placeholders, 1);
    }

    /// Marker-style phases never emit "completed" and pipelines interleave
    /// phases — once every agent inside a phase turns terminal, the phase
    /// must collapse instead of staying live forever. The collapse keeps
    /// failure visible: a phase whose only agent errored turns Failed.
    #[test]
    fn progress_running_phase_collapses_once_all_its_agents_settle() {
        let meta = json!({"runId": "wf_p", "workflowName": "demo"});
        let entries = json!([
            {"sequence": 1, "entry": {"type": "phase", "title": "构思", "state": "start"}},
            {"sequence": 2, "entry": {"type": "agent", "index": 1, "state": "start",
                "phaseTitle": "构思", "label": "idea-1", "tokens": 0, "toolCalls": 0}},
            {"sequence": 3, "entry": {"type": "agent", "index": 1, "state": "error",
                "phaseTitle": "构思", "label": "idea-1", "tokens": 0, "toolCalls": 0,
                "error": "unknown modelProfile"}},
            {"sequence": 4, "entry": {"type": "phase", "title": "评审", "state": "start"}},
            {"sequence": 5, "entry": {"type": "agent", "index": 2, "state": "start",
                "phaseTitle": "评审", "label": "review-1", "tokens": 0, "toolCalls": 0}},
        ]);
        let graph =
            WorkflowGraph::from_progress_entries(entries.as_array().expect("entries"), &meta);
        let phase = |title: &str| {
            graph
                .nodes
                .iter()
                .find(|node| {
                    node.node_type == WorkflowGraphNodeType::Phase && node.title.contains(title)
                })
                .map(|node| node.status)
                .expect("phase node")
        };
        assert_eq!(phase("构思"), WorkflowGraphNodeStatus::Failed);
        assert_eq!(phase("评审"), WorkflowGraphNodeStatus::Running);
    }

    #[test]
    fn progress_entries_build_phase_agent_graph_with_edges_and_agent_ids() {
        let meta = json!({
            "runId": "wf_1",
            "workflowName": "release-train",
            "summary": "Ship the release",
        });
        let entries = json!([
            {
                "sequence": 1,
                "entry": {"type": "phase", "title": "Research", "state": "start"},
            },
            {
                "sequence": 2,
                "entry": {
                    "type": "agent",
                    "index": 1,
                    "state": "start",
                    "phaseTitle": "Research",
                    "label": "finder",
                    "tokens": 0,
                    "toolCalls": 0,
                    "durationMs": null,
                    "error": null,
                    "agentId": null,
                },
            },
            {
                "sequence": 3,
                "entry": {"type": "phase", "title": "Research", "state": "completed"},
            },
            {
                "sequence": 4,
                "entry": {
                    "type": "agent",
                    "index": 1,
                    "state": "completed",
                    "phaseTitle": "Research",
                    "label": "finder",
                    "tokens": 1200,
                    "toolCalls": 5,
                    "durationMs": 4000,
                    "error": null,
                    "agentId": "agent-a1b2c3",
                },
            },
            {
                "sequence": 5,
                "entry": {"type": "phase", "title": "Verify", "state": "start"},
            },
            {
                "sequence": 6,
                "entry": {
                    "type": "agent",
                    "index": 2,
                    "state": "start",
                    "phaseTitle": "Verify",
                    "label": "verifier",
                    "tokens": 0,
                    "toolCalls": 0,
                    "durationMs": null,
                    "error": null,
                    "agentId": null,
                },
            },
            {
                "sequence": 7,
                "entry": {"type": "log", "message": "narrator note"},
            },
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);

        assert_eq!(graph.nodes.len(), 5); // overview + 2 phases + 2 agents
        assert_eq!(graph.nodes[0].title, "Workflow: release-train");
        assert_eq!(graph.nodes[0].detail.as_deref(), Some("Ship the release"));

        let phase_nodes: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Phase)
            .collect();
        assert_eq!(phase_nodes.len(), 2);
        assert_eq!(phase_nodes[0].status, WorkflowGraphNodeStatus::Completed);
        assert_eq!(phase_nodes[1].status, WorkflowGraphNodeStatus::Running);

        // start + completed entries fold into one agent node.
        let agents: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .collect();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].status, WorkflowGraphNodeStatus::Completed);
        assert_eq!(agents[0].agent_id.as_deref(), Some("agent-a1b2c3"));
        assert_eq!(
            agents[0].detail.as_deref(),
            Some("4000 ms · 1200 tokens · 5 tool calls")
        );
        assert_eq!(agents[1].status, WorkflowGraphNodeStatus::Running);
        assert_eq!(agents[1].agent_id, None);

        assert!(graph.edges.iter().any(|edge| {
            edge.from == "overview"
                && edge.to == "phase:0"
                && edge.kind == WorkflowGraphEdgeKind::Sequence
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:0"
                && edge.to == "phase:1"
                && edge.kind == WorkflowGraphEdgeKind::Sequence
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:0"
                && edge.to == "agent:0"
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:1"
                && edge.to == "agent:1"
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));

        // Running agents keep the overview running.
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Running);
    }

    #[test]
    fn progress_entries_failed_agent_marks_overview_failed() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {
                "sequence": 1,
                "entry": {
                    "type": "agent",
                    "index": 1,
                    "state": "error",
                    "label": "exploder",
                    "tokens": 3,
                    "toolCalls": 1,
                    "durationMs": 100,
                    "error": "boom",
                    "agentId": "agent-dead",
                },
            },
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Failed);
        let agent = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .expect("agent node");
        assert_eq!(agent.status, WorkflowGraphNodeStatus::Failed);
        assert_eq!(agent.agent_id.as_deref(), Some("agent-dead"));
        assert!(agent
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("boom")));
        // Agent outside any declared phase gets the implicit "Workflow" group.
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:0"
                && edge.to == agent.id
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));
    }

    #[test]
    fn progress_entries_empty_or_log_only_leaves_overview_only_graph() {
        let meta = json!({ "workflowName": "demo" });
        let graph = WorkflowGraph::from_progress_entries(&[], &meta);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].node_type, WorkflowGraphNodeType::Overview);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Pending);
        assert!(graph.edges.is_empty());

        let entries = json!([
            {"sequence": 1, "entry": {"type": "log", "message": "hello"}},
            {"sequence": 2, "entry": {"type": "unknown", "x": 1}},
        ]);
        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        assert_eq!(graph.nodes.len(), 1);
        assert!(graph.edges.is_empty());
        assert_eq!(graph.fallback_text.as_deref(), Some("Log: hello"));
    }

    #[test]
    fn progress_entries_phase_failure_marks_overview_failed() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 1, "entry": {"type": "phase", "title": "Validate", "state": "start"}},
            {"sequence": 2, "entry": {"type": "phase", "title": "Validate", "state": "error"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Failed);
        assert_eq!(
            graph
                .nodes
                .iter()
                .find(|node| node.node_type == WorkflowGraphNodeType::Phase)
                .expect("phase")
                .status,
            WorkflowGraphNodeStatus::Failed
        );
    }

    #[test]
    fn progress_entries_keep_same_agent_index_scoped_to_each_phase() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 1, "entry": {"type": "phase", "title": "Research", "state": "start"}},
            {"sequence": 2, "entry": {"type": "agent", "index": 1, "phaseTitle": "Research", "label": "finder", "state": "completed", "agentId": "agent-a"}},
            {"sequence": 3, "entry": {"type": "phase", "title": "Research", "state": "completed"}},
            {"sequence": 4, "entry": {"type": "phase", "title": "Verify", "state": "start"}},
            {"sequence": 5, "entry": {"type": "agent", "index": 1, "phaseTitle": "Verify", "label": "verifier", "state": "completed", "agentId": "agent-b"}},
            {"sequence": 6, "entry": {"type": "phase", "title": "Verify", "state": "completed"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let agents: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .collect();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].agent_id.as_deref(), Some("agent-a"));
        assert_eq!(agents[1].agent_id.as_deref(), Some("agent-b"));
        let contains: Vec<_> = graph
            .edges
            .iter()
            .filter(|edge| edge.kind == WorkflowGraphEdgeKind::Contains)
            .collect();
        assert_eq!(contains.len(), 2);
        assert_ne!(contains[0].from, contains[1].from);
    }

    #[test]
    fn progress_entries_keep_repeated_phase_titles_as_distinct_instances() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 1, "entry": {"type": "phase", "title": "Review", "state": "start"}},
            {"sequence": 2, "entry": {"type": "phase", "title": "Review", "state": "completed"}},
            {"sequence": 3, "entry": {"type": "phase", "title": "Review", "state": "start"}},
            {"sequence": 4, "entry": {"type": "phase", "title": "Review", "state": "completed"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let phases: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Phase)
            .collect();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].status, WorkflowGraphNodeStatus::Completed);
        assert_eq!(phases[1].status, WorkflowGraphNodeStatus::Completed);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Completed);
    }

    /// A marker-style phase (a `start` with no `completed` entry) collapses
    /// once its agents settle — and the overview must aggregate AFTER that
    /// collapse, or the root shows Running forever on a finished run.
    #[test]
    fn progress_entries_collapse_marker_phase_and_overview_when_agents_settle() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 1, "entry": {"type": "phase", "title": "Sweep", "state": "start"}},
            {"sequence": 2, "entry": {"type": "agent", "index": 1, "phaseTitle": "Sweep", "label": "finder-a", "state": "start"}},
            {"sequence": 3, "entry": {"type": "agent", "index": 2, "phaseTitle": "Sweep", "label": "finder-b", "state": "start"}},
            {"sequence": 4, "entry": {"type": "agent", "index": 1, "phaseTitle": "Sweep", "label": "finder-a", "state": "completed"}},
            {"sequence": 5, "entry": {"type": "agent", "index": 2, "phaseTitle": "Sweep", "label": "finder-b", "state": "completed"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let phase = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Phase)
            .expect("phase");
        assert_eq!(phase.status, WorkflowGraphNodeStatus::Completed);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Completed);
    }

    /// The collapse is failure-aware: a settled phase containing a failed
    /// agent turns Failed, and the failure propagates to the overview.
    #[test]
    fn progress_entries_collapse_marker_phase_to_failed_when_an_agent_fails() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 1, "entry": {"type": "phase", "title": "Sweep", "state": "start"}},
            {"sequence": 2, "entry": {"type": "agent", "index": 1, "phaseTitle": "Sweep", "label": "finder-a", "state": "start"}},
            {"sequence": 3, "entry": {"type": "agent", "index": 2, "phaseTitle": "Sweep", "label": "finder-b", "state": "start"}},
            {"sequence": 4, "entry": {"type": "agent", "index": 1, "phaseTitle": "Sweep", "label": "finder-a", "state": "completed"}},
            {"sequence": 5, "entry": {"type": "agent", "index": 2, "phaseTitle": "Sweep", "label": "finder-b", "state": "error", "error": "boom"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let phase = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Phase)
            .expect("phase");
        assert_eq!(phase.status, WorkflowGraphNodeStatus::Failed);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Failed);
    }

    #[test]
    fn progress_entries_use_phase_instance_ids_for_reentrant_same_title_phases() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 1, "entry": {"type": "phase", "title": "Review", "state": "start", "phaseInstanceId": "outer"}},
            {"sequence": 2, "entry": {"type": "agent", "index": 1, "phaseTitle": "Review", "phaseInstanceId": "outer", "label": "outer-agent", "state": "start", "agentId": "agent-outer"}},
            {"sequence": 3, "entry": {"type": "phase", "title": "Review", "state": "start", "phaseInstanceId": "inner"}},
            {"sequence": 4, "entry": {"type": "agent", "index": 2, "phaseTitle": "Review", "phaseInstanceId": "inner", "label": "inner-agent", "state": "start", "agentId": "agent-inner"}},
            {"sequence": 5, "entry": {"type": "agent", "index": 1, "phaseTitle": "Review", "phaseInstanceId": "outer", "label": "outer-agent", "state": "completed", "agentId": "agent-outer"}},
            {"sequence": 6, "entry": {"type": "phase", "title": "Review", "state": "completed", "phaseInstanceId": "outer"}},
            {"sequence": 7, "entry": {"type": "agent", "index": 2, "phaseTitle": "Review", "phaseInstanceId": "inner", "label": "inner-agent", "state": "completed", "agentId": "agent-inner"}},
            {"sequence": 8, "entry": {"type": "phase", "title": "Review", "state": "completed", "phaseInstanceId": "inner"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let phases: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Phase)
            .collect();
        assert_eq!(phases.len(), 2);
        assert!(phases
            .iter()
            .all(|phase| phase.status == WorkflowGraphNodeStatus::Completed));

        let agents: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .collect();
        assert_eq!(agents.len(), 2);
        assert!(agents
            .iter()
            .all(|agent| agent.status == WorkflowGraphNodeStatus::Completed));
        assert_eq!(agents[0].agent_id.as_deref(), Some("agent-outer"));
        assert_eq!(agents[1].agent_id.as_deref(), Some("agent-inner"));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:0"
                && edge.to == agents[0].id
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "phase:1"
                && edge.to == agents[1].id
                && edge.kind == WorkflowGraphEdgeKind::Contains
        }));
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Completed);
    }

    #[test]
    fn progress_entries_replay_wrapped_events_in_sequence_order() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 2, "entry": {"type": "agent", "index": 1, "label": "worker", "state": "completed"}},
            {"sequence": 1, "entry": {"type": "agent", "index": 1, "label": "worker", "state": "start"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let agent = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .expect("agent");
        assert_eq!(agent.status, WorkflowGraphNodeStatus::Completed);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Completed);
    }

    #[test]
    fn progress_entries_without_indexes_fold_by_label() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {"sequence": 1, "entry": {"type": "agent", "label": "alpha", "state": "start"}},
            {"sequence": 2, "entry": {"type": "agent", "label": "beta", "state": "completed"}},
            {"sequence": 3, "entry": {"type": "agent", "label": "alpha", "state": "completed"}},
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let agents: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .collect();
        assert_eq!(agents.len(), 2);
        assert!(agents
            .iter()
            .all(|agent| { agent.status == WorkflowGraphNodeStatus::Completed }));
    }

    #[test]
    fn permission_graph_bounds_all_display_fields() {
        let long = "display".repeat(2_000);
        let metadata = json!({
            "kind": "workflowReview",
            "name": long,
            "title": long,
            "description": long,
            "source": long,
            "argsSummary": long,
            "reviewText": long,
            "phases": [{ "title": long, "detail": long, "model": long }],
            "calls": [{ "kind": "agent", "line": 1, "summary": long, "phase": long }],
            "warnings": [long],
            "errors": [long],
        });

        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("permission graph");

        assert!(graph
            .nodes
            .iter()
            .all(|node| node.title.chars().count() <= 512));
        assert!(graph.nodes.iter().all(|node| {
            node.detail
                .as_deref()
                .is_none_or(|detail| detail.chars().count() <= 2_048)
        }));
        assert!(graph
            .fallback_text
            .as_deref()
            .is_some_and(|text| text.chars().count() <= 8_192));
    }

    #[test]
    fn progress_graph_bounds_error_log_and_identity_fields() {
        let long = "runtime".repeat(2_000);
        let meta = json!({
            "workflowName": long,
            "summary": long,
        });
        let mut entries = vec![
            json!({"sequence": 1, "entry": {"type": "phase", "title": long, "state": "start", "phaseInstanceId": long}}),
            json!({"sequence": 2, "entry": {"type": "agent", "index": 1, "phaseTitle": long, "phaseInstanceId": long, "label": long, "state": "error", "error": long, "agentId": long}}),
        ];
        entries.extend((3..43).map(
            |sequence| json!({"sequence": sequence, "entry": {"type": "log", "message": long}}),
        ));

        let graph = WorkflowGraph::from_progress_entries(&entries, &meta);

        assert!(graph
            .nodes
            .iter()
            .all(|node| node.title.chars().count() <= 512));
        assert!(graph.nodes.iter().all(|node| {
            node.detail
                .as_deref()
                .is_none_or(|detail| detail.chars().count() <= 2_048)
        }));
        assert!(graph.nodes.iter().all(|node| {
            node.agent_id
                .as_deref()
                .is_none_or(|agent_id| agent_id.chars().count() <= 256)
        }));
        assert!(graph
            .fallback_text
            .as_deref()
            .is_some_and(|text| text.chars().count() <= 8_192));
    }

    #[test]
    fn progress_entries_missing_agent_id_keeps_node_unlinked() {
        let meta = json!({ "workflowName": "demo" });
        let entries = json!([
            {
                "sequence": 1,
                "entry": {
                    "type": "agent",
                    "index": 1,
                    "state": "completed",
                    "label": "legacy",
                    "tokens": 0,
                    "toolCalls": 0,
                    "durationMs": null,
                    "error": null,
                },
            },
        ]);

        let graph = WorkflowGraph::from_progress_entries(entries.as_array().unwrap(), &meta);
        let agent = graph
            .nodes
            .iter()
            .find(|node| node.node_type == WorkflowGraphNodeType::Agent)
            .expect("agent node");
        assert_eq!(agent.agent_id, None);
        assert_eq!(graph.nodes[0].status, WorkflowGraphNodeStatus::Completed);
    }
}

/// Fold one `phase` entry into the graph.
///
/// An entry the loop cannot use returns early, which is what the outer
/// `continue` did: the entry match is the last statement in the loop body, so
/// skipping the rest of the arm and skipping the rest of the iteration are the
/// same thing.
fn apply_phase_entry(
    entry: &serde_json::Map<String, Value>,
    graph: &mut WorkflowGraph,
    phases: &mut Vec<PhaseRecord>,
    active_phases: &mut Vec<usize>,
) {
    let Some(title) = string_field(entry, "title") else {
        return;
    };
    let state = string_field(entry, "state").unwrap_or_else(|| "start".to_string());
    let status = WorkflowGraphNodeStatus::from_runtime_state(&state);
    let key = normalized_key(&title);
    let id_key = string_field(entry, "phaseInstanceId").or_else(|| string_field(entry, "phaseId"));
    let starts_new_instance = matches!(state.as_str(), "start" | "started");
    let existing_by_id = id_key.as_ref().and_then(|id| {
        phases
            .iter()
            .position(|phase| phase.id_key.as_ref() == Some(id))
    });
    let phase_idx = if let Some(phase_idx) = existing_by_id {
        phase_idx
    } else if starts_new_instance {
        let id = format!("phase:{}", phases.len());
        graph.nodes.push(WorkflowGraphNode {
            id,
            title: format!("Phase {}: {title}", phases.len() + 1),
            detail: None,
            node_type: WorkflowGraphNodeType::Phase,
            status,
            editable: false,
            modified: false,
            agent_id: None,
            agent: None,
        });
        let phase_idx = phases.len();
        phases.push(PhaseRecord {
            key,
            id_key,
            node_idx: graph.nodes.len() - 1,
            open: true,
            implicit: false,
        });
        active_phases.push(phase_idx);
        phase_idx
    } else if let Some(phase_idx) = phases
        .iter()
        .enumerate()
        .rev()
        .find(|(_, phase)| phase.key == key && phase.open)
        .map(|(phase_idx, _)| phase_idx)
        .or_else(|| {
            phases
                .iter()
                .enumerate()
                .rev()
                .find(|(_, phase)| phase.key == key)
                .map(|(phase_idx, _)| phase_idx)
        })
    {
        phase_idx
    } else {
        let id = format!("phase:{}", phases.len());
        graph.nodes.push(WorkflowGraphNode {
            id,
            title: format!("Phase {}: {title}", phases.len() + 1),
            detail: None,
            node_type: WorkflowGraphNodeType::Phase,
            status,
            editable: false,
            modified: false,
            agent_id: None,
            agent: None,
        });
        let phase_idx = phases.len();
        phases.push(PhaseRecord {
            key,
            id_key,
            node_idx: graph.nodes.len() - 1,
            open: status == WorkflowGraphNodeStatus::Running,
            implicit: false,
        });
        if status == WorkflowGraphNodeStatus::Running {
            active_phases.push(phase_idx);
        }
        phase_idx
    };

    graph.nodes[phases[phase_idx].node_idx].status = status;
    if matches!(
        status,
        WorkflowGraphNodeStatus::Completed | WorkflowGraphNodeStatus::Failed
    ) {
        phases[phase_idx].open = false;
        if let Some(position) = active_phases
            .iter()
            .rposition(|active| *active == phase_idx)
        {
            active_phases.remove(position);
        }
    } else if status == WorkflowGraphNodeStatus::Running && !active_phases.contains(&phase_idx) {
        phases[phase_idx].open = true;
        active_phases.push(phase_idx);
    }
}

/// Fold one `agent` entry into the graph.
///
/// Agents are matched to a phase three ways -- by id, by (phase, index) and by
/// (phase, label) -- because the progress stream has carried all three shapes
/// over time and a run may interleave them.
#[allow(clippy::too_many_arguments)]
fn apply_agent_entry(
    entry: &serde_json::Map<String, Value>,
    graph: &mut WorkflowGraph,
    phases: &mut Vec<PhaseRecord>,
    active_phases: &mut [usize],
    agents: &mut Vec<AgentRecord>,
    agents_by_id: &mut HashMap<String, usize>,
    agents_by_index: &mut HashMap<(usize, u64), usize>,
    agents_by_label: &mut HashMap<(usize, String), usize>,
    global_agents_by_index: &mut HashMap<u64, Option<usize>>,
) {
    let index = entry.get("index").and_then(Value::as_u64).unwrap_or(0);
    let state = string_field(entry, "state").unwrap_or_else(|| "start".to_string());
    let label = string_field(entry, "label").unwrap_or_else(|| "agent".to_string());
    let label_key = normalized_key(&label);
    let agent_id = string_field(entry, "agentId");
    let explicit_phase = string_field(entry, "phaseTitle");
    let explicit_phase_id =
        string_field(entry, "phaseInstanceId").or_else(|| string_field(entry, "phaseId"));
    let has_phase_context =
        explicit_phase.is_some() || explicit_phase_id.is_some() || !active_phases.is_empty();
    let phase_key = explicit_phase.as_deref().map(normalized_key);
    let existing_record_idx = agent_id
        .as_ref()
        .and_then(|agent_id| agents_by_id.get(agent_id).copied());
    let phase_idx = existing_record_idx
        .map(|record_idx| agents[record_idx].phase_idx)
        .or_else(|| {
            explicit_phase_id.as_ref().and_then(|id| {
                phases
                    .iter()
                    .position(|phase| phase.id_key.as_ref() == Some(id))
            })
        })
        .or_else(|| {
            phase_key.as_deref().and_then(|key| {
                phases
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, phase)| phase.key == key && phase.open)
                    .map(|(phase_idx, _)| phase_idx)
                    .or_else(|| {
                        phases
                            .iter()
                            .enumerate()
                            .rev()
                            .find(|(_, phase)| phase.key == key)
                            .map(|(phase_idx, _)| phase_idx)
                    })
            })
        })
        .or_else(|| active_phases.last().copied())
        .or_else(|| {
            if explicit_phase.is_none() {
                phases
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, phase)| phase.implicit && phase.key == "workflow")
                    .map(|(phase_idx, _)| phase_idx)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            let title = explicit_phase
                .clone()
                .unwrap_or_else(|| "Workflow".to_string());
            let id = format!("phase:{}", phases.len());
            graph.nodes.push(WorkflowGraphNode {
                id,
                title: format!("Phase {}: {title}", phases.len() + 1),
                detail: None,
                node_type: WorkflowGraphNodeType::Phase,
                status: WorkflowGraphNodeStatus::Info,
                editable: false,
                modified: false,
                agent_id: None,
                agent: None,
            });
            let phase_idx = phases.len();
            phases.push(PhaseRecord {
                key: normalized_key(&title),
                id_key: explicit_phase_id.clone(),
                node_idx: graph.nodes.len() - 1,
                open: false,
                implicit: true,
            });
            phase_idx
        });

    let mut record_idx = existing_record_idx;
    if record_idx.is_none() {
        record_idx = if index > 0 {
            agents_by_index
                .get(&(phase_idx, index))
                .copied()
                .or_else(|| {
                    agents_by_label
                        .get(&(phase_idx, label_key.clone()))
                        .copied()
                        .filter(|record_idx| agents[*record_idx].index == 0)
                })
        } else {
            agents_by_label
                .get(&(phase_idx, label_key.clone()))
                .copied()
        };
    }
    if record_idx.is_none() && !has_phase_context && index > 0 {
        record_idx = global_agents_by_index.get(&index).copied().flatten();
    }

    let record_idx = match record_idx {
        Some(record_idx) => record_idx,
        None => {
            let node_id = format!("agent:{}", agents.len());
            graph.nodes.push(WorkflowGraphNode {
                id: node_id,
                title: label.clone(),
                detail: None,
                node_type: WorkflowGraphNodeType::Agent,
                status: WorkflowGraphNodeStatus::Pending,
                editable: false,
                modified: false,
                agent_id: None,
                agent: None,
            });
            let record_idx = agents.len();
            agents.push(AgentRecord {
                phase_idx,
                node_idx: graph.nodes.len() - 1,
                index,
                label_key: label_key.clone(),
                order: record_idx,
            });
            record_idx
        }
    };

    let record = &mut agents[record_idx];
    if record.index == 0 && index > 0 {
        record.index = index;
    }
    record.label_key = label_key.clone();
    if index > 0 {
        agents_by_index.insert((record.phase_idx, index), record_idx);
        match global_agents_by_index.get(&index).copied() {
            None => {
                global_agents_by_index.insert(index, Some(record_idx));
            }
            Some(Some(existing)) if existing != record_idx => {
                global_agents_by_index.insert(index, None);
            }
            _ => {}
        }
    }
    agents_by_label.insert((record.phase_idx, label_key), record_idx);
    let node = &mut graph.nodes[record.node_idx];
    node.status = WorkflowGraphNodeStatus::from_runtime_state(&state);
    node.title = label;
    if let Some(agent_id) = agent_id {
        node.agent_id = Some(agent_id.clone());
        agents_by_id.insert(agent_id, record_idx);
    }
    let mut parts = Vec::new();
    if let Some(duration) = entry.get("durationMs").and_then(Value::as_u64) {
        parts.push(format!("{duration} ms"));
    }
    let tokens = entry.get("tokens").and_then(Value::as_u64).unwrap_or(0);
    if tokens > 0 {
        parts.push(format!("{tokens} tokens"));
    }
    let tool_calls = entry.get("toolCalls").and_then(Value::as_u64).unwrap_or(0);
    if tool_calls > 0 {
        parts.push(format!("{tool_calls} tool calls"));
    }
    if let Some(error) = string_field(entry, "error") {
        parts.push(error);
    }
    node.detail = (!parts.is_empty()).then(|| parts.join(" · "));
}
