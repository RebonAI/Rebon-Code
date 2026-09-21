use super::*;
use crate::turn_hook::{QueryEventSender, TurnHookContext};
use rebon_api::SessionHandleId;
use std::collections::HashMap;
use std::sync::{Mutex, Weak};

/// Code Mode 在工具投影中的名称，与内核注册名由漂移测试保持一致。
pub const RUN_CODE_TOOL: &str = "run_code";
const INSTRUCTIONS: &str = "# Code Mode\n\
Prefer `run_code` as the default tool-use interface whenever it is available in the current tool set. \
Keep this preference across subsequent turns and context compaction while the tool remains available. \
Combine independent reads or searches in one program; await dependent operations in order, including \
reading a file before editing it. Use direct tools when Code Mode is unavailable or unsuitable. \
This is a preference, not a requirement to force tool calls. All tool restrictions, schema discovery, \
permissions and user approvals still apply to every nested call; never use Code Mode to bypass a denial.";
const UNAVAILABLE: &str = "# Code Mode\n\
`run_code` is currently unavailable in this query. Do not attempt to use it or apply the Code Mode \
preference while it is unavailable; use the other currently available tools instead.";

/// 只记住首次请求的提示放置位置；可用性始终来自当前工具解析器。
#[derive(Default)]
pub(crate) struct CodeModePromptCache {
    sessions: Mutex<HashMap<SessionHandleId, (Weak<SessionHandle>, bool)>>,
}

impl CodeModePromptCache {
    fn in_system(&self, session: &Arc<SessionHandle>, initial: bool) -> bool {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|_, (handle, _)| handle.strong_count() > 0);
        sessions
            .entry(session.id())
            .or_insert_with(|| (Arc::downgrade(session), initial))
            .1
    }
}

pub(super) fn fresh_history(history: &[ApiMessage]) -> bool {
    !history.iter().any(|message| {
        message.role == Role::Assistant
            || message.content.iter().any(|block| {
                block
                    .as_text()
                    .is_some_and(|text| text.contains(COMPACT_SUMMARY_MARKER))
            })
    })
}

fn refresh_tool(engine: &Engine, params: &mut QueryParams, context: &ToolContext) -> bool {
    let mut filter = combine_filters(
        params
            .effective_tool_filter
            .as_ref()
            .or(params.base_tool_filter.as_ref()),
        params.execution_policy.as_ref(),
    );
    filter = combine_filters(filter.as_ref(), params.invariant_execution_policy.as_ref());
    filter = combine_filters(filter.as_ref(), context.execution_policy());
    if let Some(context_filter) = context.tool_filter() {
        filter = Some(match filter {
            Some(filter) => filter.intersect(context_filter),
            None => context_filter.clone(),
        });
    }
    let tool = if params.capability_mode.is_normal() {
        engine
            .tool_resolver_for_context(context)
            .resolve(RUN_CODE_TOOL, filter.as_ref())
            .ok()
            .flatten()
    } else {
        None
    };
    let Some(tool) = tool else {
        params.tools.retain(|tool| tool.name != RUN_CODE_TOOL);
        return false;
    };
    let schema = ApiTool {
        name: RUN_CODE_TOOL.into(),
        description: tool.model_description().into(),
        input_schema: tool.input_schema(),
    };
    if let Some(existing) = params
        .tools
        .iter_mut()
        .find(|tool| tool.name == RUN_CODE_TOOL)
    {
        *existing = schema;
    } else {
        params.tools.push(schema);
    }
    true
}

pub(super) fn prepare(
    engine: &Engine,
    session: &Arc<SessionHandle>,
    params: &mut QueryParams,
    context: &ToolContext,
    history: &[ApiMessage],
    fresh_history: bool,
) -> Option<CodeModeNotification> {
    let available = refresh_tool(engine, params, context);
    let in_system = engine
        .code_mode_prompts
        .in_system(session, fresh_history && available);
    if in_system {
        let system = params.system.get_or_insert_with(String::new);
        if !system.contains(INSTRUCTIONS) {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(INSTRUCTIONS);
        }
    }

    // 从实际历史判断最近一次通知；压缩移除通知后自然补发，不缓存 enabled 副本。
    let latest = history
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| m.content.iter().rev())
        .filter_map(ApiContentBlock::as_text)
        .find_map(|text| {
            let body = text
                .strip_prefix("<system-reminder>\n")?
                .strip_suffix("\n</system-reminder>")?;
            if body == INSTRUCTIONS {
                Some(true)
            } else if body == UNAVAILABLE {
                Some(false)
            } else {
                None
            }
        });
    match (available, latest) {
        (true, Some(false)) => Some(CodeModeNotification(INSTRUCTIONS)),
        (true, None) if !in_system => Some(CodeModeNotification(INSTRUCTIONS)),
        (false, Some(true)) => Some(CodeModeNotification(UNAVAILABLE)),
        (false, None) if in_system => Some(CodeModeNotification(UNAVAILABLE)),
        _ => None,
    }
}

pub(super) struct CodeModeNotification(&'static str);

impl AttachmentPoller for CodeModeNotification {
    fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        vec![ApiMessage::user_text(format!(
            "<system-reminder>\n{}\n</system-reminder>",
            self.0
        ))]
    }
}

impl CodeModeNotification {
    pub(super) fn dispatch(
        self,
        tx: &QueryEventSender,
        history: &[ApiMessage],
        params: &QueryParams,
        iteration: usize,
    ) -> TurnHookContext {
        let (session_id, turn_id) = params
            .attachment_poller
            .as_ref()
            .map(|binding| (binding.session_id.clone(), binding.turn_id.clone()))
            .unwrap_or_default();
        let binding = AttachmentPollerBinding::new(Arc::new(self), session_id, turn_id);
        tx.dispatch_attachment_poll(
            AttachmentPollPhase::Eager,
            iteration as u64,
            history,
            Some(&binding),
        )
    }
}
