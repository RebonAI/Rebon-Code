use super::*;
use rebon_render::{
    attachment::{
        AttachmentLineDisplay, AttachmentProjection, AttachmentRelevantMemoriesDisplay,
        AttachmentRelevantMemoryRow, AttachmentTaskStatusDisplay, AttachmentTeammateMailboxDisplay,
        AttachmentTeammateMailboxItemDisplay, AttachmentTone,
    },
    plan_approval::{
        PlanApprovalRenderable, PlanApprovalRequestDisplay, PlanApprovalResponseDisplay,
    },
    system_text::{
        MemoryFileRowStyleHint, StopHookSummaryDisplay, SystemBridgeStatusDisplay,
        SystemGenericTextDisplay, SystemMemorySavedDisplay, SystemMemorySavedEntry,
        SystemTextProjection, SystemTurnBudgetDisplay, SystemTurnDurationDisplay,
        SystemVisualMarker,
    },
    teammate_messages::TeammateMessageContentDisplay,
};

fn lines(text: Text<'static>) -> Vec<String> {
    text.lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn attachment_and_system_text_renderers_emit_readable_lines() {
    let attachment = render_attachment_projection(
        &AttachmentProjection::RelevantMemories(AttachmentRelevantMemoriesDisplay {
            margin_top: 1,
            background: None,
            count: 2,
            count_word: "memories",
            show_expand_hint: true,
            show_entries: true,
            gutter_width: 2,
            transcript_padding_left: 5,
            entries: vec![
                AttachmentRelevantMemoryRow {
                    path: "/tmp/a.md".into(),
                    basename: "a.md".into(),
                    transcript_body: None,
                },
                AttachmentRelevantMemoryRow {
                    path: "/tmp/b.md".into(),
                    basename: "b.md".into(),
                    transcript_body: Some("remember".into()),
                },
            ],
        }),
        &MessagesRenderTheme::plain(),
    );
    let rendered = lines(attachment);
    assert!(rendered[0].contains("Recalled 2 memories"));
    assert!(rendered[0].contains("Ctrl+O"));
    assert!(rendered[2].contains("remember"));

    let line_attachment = render_attachment_projection(
        &AttachmentProjection::Lines(vec![AttachmentLineDisplay {
            text: "hook failed".into(),
            tone: Some(AttachmentTone::Error),
            dim: true,
            background: None,
        }]),
        &MessagesRenderTheme::plain(),
    );
    assert_eq!(lines(line_attachment), vec!["hook failed"]);

    let task = render_attachment_projection(
        &AttachmentProjection::TaskStatus(AttachmentTaskStatusDisplay::Teammate {
            background: None,
            dot_glyph: "o".into(),
            agent_name: "alice".into(),
            agent_color: Some("red".into()),
            status_text: "shut down gracefully".into(),
        }),
        &MessagesRenderTheme::plain(),
    );
    assert_eq!(lines(task), vec!["o Teammate @alice shut down gracefully"]);

    let turn = render_system_text_projection(
        &SystemTextProjection::TurnDuration(SystemTurnDurationDisplay {
            margin_top: 1,
            background: None,
            marker: SystemVisualMarker::TeardropAsterisk,
            duration_text: Some("Cooked for 1m 5s".into()),
            budget: Some(SystemTurnBudgetDisplay {
                usage_text: "1.3k / 10.0k (13%)".into(),
                prefixed_with_separator: true,
                nudges_text: Some("2 nudges".into()),
            }),
            background_task_summary: Some("2 local agents still running".into()),
        }),
        &MessagesRenderTheme::plain(),
    );
    assert_eq!(
        lines(turn),
        vec!["Cooked for 1m 5s · 1.3k / 10.0k (13%) · 2 nudges · 2 local agents still running"]
    );

    let bridge = render_system_text_projection(
        &SystemTextProjection::BridgeStatus(SystemBridgeStatusDisplay {
            margin_top: 1,
            background: None,
            intro: "Remote Control compatibility is active. Code in CLI or at",
            url: "https://x".into(),
            upgrade_nudge: Some("upgrade app".into()),
            width: 999,
        }),
        &MessagesRenderTheme::plain(),
    );
    let bridge_lines = lines(bridge);
    assert_eq!(
        bridge_lines[0],
        "Remote Control compatibility is active. Code in CLI or at"
    );
    assert_eq!(bridge_lines[1], "https://x");
    assert_eq!(bridge_lines[2], "upgrade app");

    let stop = render_system_text_projection(
        &SystemTextProjection::StopHookSummary(StopHookSummaryDisplay::Default {
            margin_top: 1,
            background: None,
            marker: SystemVisualMarker::BlackCircle,
            summary: "Ran 2 stop hooks".into(),
            detail_lines: vec!["- lint (0.5s)".into()],
            prevented_line: Some("Stopped".into()),
            error_lines: vec!["Stop hook error: bad".into()],
            show_expand_hint: true,
            width: 90,
        }),
        &MessagesRenderTheme::plain(),
    );
    let stop_lines = lines(stop);
    assert!(stop_lines.iter().any(|line| line.contains("Ctrl+O")));
    assert!(stop_lines.iter().any(|line| line.contains("Stopped")));
    assert!(stop_lines.iter().any(|line| line.contains("bad")));
    assert!(stop_lines
        .iter()
        .any(|line| line.starts_with("Ran 2 stop hooks")));
}

#[test]
fn teammate_mailbox_renderer_expands_structured_items() {
    let rendered = render_attachment_projection(
        &AttachmentProjection::TeammateMailbox(AttachmentTeammateMailboxDisplay {
            items: vec![
                AttachmentTeammateMailboxItemDisplay::TaskAssignment {
                    dot_glyph: "o".into(),
                    task_id: "17".into(),
                    subject: None,
                    from: "lead".into(),
                },
                AttachmentTeammateMailboxItemDisplay::PlanApproval(
                    PlanApprovalRenderable::Request(PlanApprovalRequestDisplay {
                        title: "Plan Approval Request from alice".into(),
                        plan_content: "step 1\nstep 2".into(),
                        plan_file_path: "/tmp/plan.md".into(),
                    }),
                ),
                AttachmentTeammateMailboxItemDisplay::PlanApproval(
                    PlanApprovalRenderable::Response(PlanApprovalResponseDisplay::Rejected {
                        title: "Plan Rejected by bob".into(),
                        feedback: Some("add tests".into()),
                        footer: "Please revise your plan based on the feedback and call ExitPlanMode again.",
                    }),
                ),
                AttachmentTeammateMailboxItemDisplay::Plain(TeammateMessageContentDisplay {
                    display_name: "alice".into(),
                    color: Some("red".into()),
                    content: "full body".into(),
                    summary: Some("Brief update".into()),
                    is_transcript_mode: true,
                }),
            ],
        }),
        &MessagesRenderTheme::plain(),
    );

    assert_eq!(
        lines(rendered),
        vec![
            "o Task assigned: #17 (from lead)",
            "Plan Approval Request from alice",
            "step 1",
            "step 2",
            "Plan file: /tmp/plan.md",
            "Plan Rejected by bob",
            "Feedback: add tests",
            "Please revise your plan based on the feedback and call ExitPlanMode again.",
            "@alice\u{276f} Brief update",
            "  full body",
        ]
    );
}

#[test]
fn system_generic_and_memory_saved_renderers_keep_visible_structure() {
    let generic = render_system_text_projection(
        &SystemTextProjection::Generic(SystemGenericTextDisplay {
            margin_top: 1,
            background: None,
            marker: Some(SystemVisualMarker::ReferenceMark),
            color: Some("warning".into()),
            dim_color: false,
            content: "watch out".into(),
            width: Some(80),
        }),
        &MessagesRenderTheme::plain(),
    );
    assert_eq!(lines(generic), vec!["watch out"]);

    let memory = render_system_text_projection(
        &SystemTextProjection::MemorySaved(SystemMemorySavedDisplay {
            margin_top: 1,
            background: None,
            marker: SystemVisualMarker::BlackCircle,
            verb: "Saved".into(),
            parts: vec!["2 memories".into(), "1 team memory".into()],
            entries: vec![SystemMemorySavedEntry {
                path: "/tmp/a.md".into(),
                basename: "a.md".into(),
                open_path: "/tmp/a.md".into(),
                idle_style: MemoryFileRowStyleHint {
                    dim: true,
                    underline: false,
                },
                hover_style: MemoryFileRowStyleHint {
                    dim: false,
                    underline: true,
                },
            }],
        }),
        &MessagesRenderTheme::plain(),
    );
    assert_eq!(
        lines(memory),
        vec!["Saved 2 memories · 1 team memory", "  a.md"]
    );
}
