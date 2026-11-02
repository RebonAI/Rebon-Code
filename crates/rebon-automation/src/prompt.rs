//! Prompt templates: `{{var}}` placeholders a rule's prompt is rendered with.

/// Values substituted into a rule's prompt template when it fires.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptContext {
    pub project: String,
    pub cwd: String,
    pub date: String,
    pub time: String,
    pub trigger: String,
    pub session_title: Option<String>,
    pub job_summary: Option<String>,
    pub job_status: Option<String>,
}

/// Variable names a template may use, in the order an editor offers them.
pub const TEMPLATE_VARS: [&str; 8] = [
    "project",
    "cwd",
    "date",
    "time",
    "trigger",
    "session_title",
    "job_summary",
    "job_status",
];

/// Replace `{{var}}` placeholders. Unknown variables are left verbatim so a
/// typo is visible in the launched prompt instead of silently vanishing.
pub fn render_prompt(template: &str, ctx: &PromptContext) -> String {
    let values: [&str; 8] = [
        &ctx.project,
        &ctx.cwd,
        &ctx.date,
        &ctx.time,
        &ctx.trigger,
        ctx.session_title.as_deref().unwrap_or(""),
        ctx.job_summary.as_deref().unwrap_or(""),
        ctx.job_status.as_deref().unwrap_or(""),
    ];
    let mut out = template.to_string();
    for (name, value) in TEMPLATE_VARS.iter().zip(values) {
        out = out.replace(&format!("{{{{{name}}}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_var_is_replaced_and_unknown_ones_kept() {
        let ctx = PromptContext {
            project: "repo".into(),
            cwd: "/w/repo".into(),
            date: "2026-11-02".into(),
            time: "09:00".into(),
            trigger: "Daily 09:00".into(),
            session_title: Some("fix login".into()),
            job_summary: Some("tests failed".into()),
            job_status: Some("failed".into()),
        };
        let template = TEMPLATE_VARS
            .iter()
            .map(|name| format!("{{{{{name}}}}}"))
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(
            render_prompt(&template, &ctx),
            "repo|/w/repo|2026-11-02|09:00|Daily 09:00|fix login|tests failed|failed"
        );
        assert_eq!(render_prompt("{{nope}} {{project}}", &ctx), "{{nope}} repo");
    }

    #[test]
    fn event_vars_render_empty_for_a_clock_rule() {
        let ctx = PromptContext::default();
        assert_eq!(
            render_prompt("[{{session_title}}][{{job_summary}}][{{job_status}}]", &ctx),
            "[][][]"
        );
    }
}
