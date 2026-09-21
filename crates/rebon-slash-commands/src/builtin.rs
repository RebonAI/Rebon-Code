//! The built-in slash commands, as data.
//!
//! The kernel's command seat registers every row at boot; the readers in this
//! crate fall back to this table only while no seat is installed (tests,
//! headless tools), so a binary without a booted kernel still knows its own
//! commands. Plugin commands never appear here — they are registered at run
//! time and only the seat sees them.

use crate::{Category, CommandKind, CommandSpec, Surfaces};

/// Every built-in command, in menu order.
///
/// The order is a product judgement, not an accident: the things people reach
/// for first, then settings surfaces, then model-facing commands. A front end
/// showing a picker for an empty query shows this order.
///
/// `surfaces` records where a command actually does something. A command a
/// front end cannot run is still parsed there, so typing it produces an
/// explanation instead of silently becoming prompt text for the model, which
/// is what happened before the surface bits existed. The `WEB`, `MOBILE` and
/// `SESSION_CONTROL` bits are the three lists that used to be written out by
/// hand: the web page's commands, the desktop's relay surface, and the TUI's
/// mirror forwarding gate.
///
/// Skills and plugin-registered commands are **not** here. They are
/// registered on the same seat at run time; this table is only what ships in
/// the binary.
pub fn builtin_command_table() -> Vec<CommandSpec> {
    use CommandKind::{Panel, Prompt, Session};
    const LOCAL_WEB: Surfaces = Surfaces::LOCAL.with(Surfaces::WEB);
    const CONTROL: Surfaces = Surfaces::SESSION_CONTROL;
    const ALL_EVERYWHERE: Surfaces = Surfaces::ALL
        .with(Surfaces::WEB)
        .with(Surfaces::MOBILE)
        .with(CONTROL);

    vec![
        CommandSpec::new("help", "Show available commands")
            .zh_aliases(["帮助"])
            .surfaces(LOCAL_WEB)
            .kind(Panel),
        CommandSpec::new(
            "new",
            "Start a fresh conversation (clear history + reset session)",
        )
        .zh_aliases(["新建", "新会话"])
        .surfaces(LOCAL_WEB),
        CommandSpec::new("clear", "Start a new conversation (same as /new)")
            .zh_aliases(["清空", "清除"])
            .surfaces(LOCAL_WEB),
        CommandSpec::new("status", "Show session, model, and project status")
            .zh_aliases(["状态"])
            .surfaces(ALL_EVERYWHERE)
            .kind(Session),
        CommandSpec::new(
            "cost",
            "Local estimate only: session duration and approximate token usage",
        )
        .zh_aliases(["费用", "用量"])
        .surfaces(ALL_EVERYWHERE)
        .kind(Session),
        CommandSpec::new("stop", "Interrupt the running turn")
            .zh_aliases(["停止", "中断"])
            .surfaces(LOCAL_WEB),
        CommandSpec::new("codemode", "开启或关闭当前会话的实验性 Code Mode")
            .hint("[on|off]")
            .surfaces(ALL_EVERYWHERE)
            .kind(Session),
        CommandSpec::new("effort", "Show or set this session's reasoning effort")
            .zh_aliases(["推理强度", "思考强度"])
            .hint("[xhigh|high|medium|low|auto]")
            .surfaces(LOCAL_WEB)
            .kind(Panel),
        CommandSpec::new(
            "rewind",
            "Restore the code and/or conversation to a previous point",
        )
        .aliases(["checkpoint"])
        .zh_aliases(["回溯", "检查点"])
        .surfaces(LOCAL_WEB)
        .kind(Panel),
        CommandSpec::new("settings", "Open settings")
            .aliases(["config"])
            .zh_aliases(["设置"])
            .surfaces(LOCAL_WEB)
            .kind(Panel),
        CommandSpec::new("model", "List or switch models for the active provider")
            .zh_aliases(["模型"])
            .hint("[model|list]")
            .surfaces(LOCAL_WEB)
            .kind(Panel),
        CommandSpec::new(
            "provider",
            "Configure providers with a protected credential form",
        )
        .zh_aliases(["服务商", "供应商"])
        .hint("add <preset> <apiKey>|add|remove|list|use|add-model")
        .kind(Panel),
        // `/profile` is not here: the profile plugin registers it on the
        // command seat beside the two tools that propose the same change.
        // Turning that plugin off takes the command with it.
        CommandSpec::new("theme", "Change the color theme")
            .zh_aliases(["主题", "配色"])
            .surfaces(LOCAL_WEB)
            .kind(Panel),
        // `/skills` is not here: the skill plugin registers it on the command
        // seat beside the selector it opens. Turning that plugin off takes the
        // command with it.
        CommandSpec::new(
            "mcp",
            "Show configured MCP servers and loaded tools; reconnect or disconnect one",
        )
        .zh_aliases(["连接器"])
        .surfaces(ALL_EVERYWHERE)
        .kind(Session),
        CommandSpec::new("plugin", "Install, enable, disable, and list local plugins")
            .aliases(["plugins"])
            .zh_aliases(["插件"])
            .hint("install <path|name|rust-lsp> [--scope user|project]")
            .surfaces(LOCAL_WEB)
            .kind(Panel),
        // `/migrate` is not here: the import it runs belongs to the onboarding
        // plugin, and so does the wizard step it opens.
        CommandSpec::new(
            "permissions",
            "Review auto-mode denials (approve, retry, clear)",
        )
        .zh_aliases(["权限"])
        .hint("[approve <id>|retry <id>|clear [all]]")
        .surfaces(LOCAL_WEB.with(CONTROL))
        .kind(Session),
        CommandSpec::new("shortcuts", "Show keyboard shortcuts")
            .zh_aliases(["快捷键"])
            .surfaces(Surfaces::DESKTOP_ONLY.with(Surfaces::WEB))
            .kind(Panel),
        CommandSpec::new("runtime", "Configure the plugin and Node runtime")
            .zh_aliases(["运行时"])
            .surfaces(Surfaces::DESKTOP_ONLY)
            .kind(Panel),
        CommandSpec::new(
            "automation",
            "Manage scheduled and event-triggered automations",
        )
        .zh_aliases(["自动化"])
        .surfaces(Surfaces::DESKTOP_ONLY)
        .kind(Panel),
        CommandSpec::new("devices", "Manage paired mobile devices")
            .zh_aliases(["设备"])
            .surfaces(Surfaces::DESKTOP_ONLY)
            .kind(Panel),
        CommandSpec::new("review", "Ask the model to review changes or a PR")
            .zh_aliases(["审查", "代码审查"])
            .kind(Prompt),
        CommandSpec::new("ultrawork", "Run workflow orchestration for complex tasks")
            .aliases(["ulw"])
            .hint("<request>")
            .surfaces(Surfaces::ALL.with(Surfaces::WEB))
            .kind(Prompt),
        CommandSpec::new(
            "ultraplan",
            "Multi-agent plan mode: draft a plan, review it, then implement",
        )
        .zh_aliases(["计划"])
        .hint("[--grill] [--file <plan.md>] <prompt>")
        .kind(Prompt),
        CommandSpec::new(
            "grill",
            "Start a strict one-question-at-a-time ultraplan interview",
        )
        .hint("[--file <plan.md>] <prompt>")
        .kind(Prompt),
        CommandSpec::new(
            "ceo",
            "Toggle CEO/coordinator mode; orchestrate workers for complex tasks",
        )
        .hint("[on|off|task]")
        .kind(Prompt),
        // Added rather than reusing /agent, which already means "spawn a
        // sub-agent with this prompt". The two meanings had been sharing that
        // one command, and the spawn side took every input — so switching was
        // unreachable and `/agent list` spawned a sub-agent whose prompt was
        // the word "list". Changing what /agent means is a decision for
        // whoever owns the product; adding an unambiguous name for the other
        // meaning is not.
        CommandSpec::new(
            "backend",
            "Run this session on rebon's engine, an ACP agent, or a kernel loop",
        )
        .zh_aliases(["切换引擎", "后端"])
        .hint("[list|reconnect [id]|<agent-id>|local]")
        .category(Category::Agent)
        .surfaces(LOCAL_WEB.with(CONTROL))
        .kind(Session),
        CommandSpec::new(
            "agent",
            "Spawn a background sub-agent, or switch backend (see /backend)",
        )
        .zh_aliases(["子代理"])
        .hint("<prompt> | <agent-id>")
        .category(Category::Agent)
        .surfaces(LOCAL_WEB),
        CommandSpec::new("compact", "Compact the conversation now and keep working")
            .zh_aliases(["压缩"])
            .hint("[instructions]")
            .surfaces(LOCAL_WEB.with(CONTROL))
            .kind(Session),
        CommandSpec::new("context", "Show context window usage and message breakdown")
            .zh_aliases(["上下文"])
            .surfaces(ALL_EVERYWHERE)
            .kind(Session),
        CommandSpec::new("prune", "Inspect or change context pruning")
            .zh_aliases(["修剪"])
            .hint("context|stats|sweep [n]|manual [on|off]")
            .surfaces(Surfaces::LOCAL.with(CONTROL))
            .kind(Session),
        // `/memory` is not here: the memory plugin registers it on the command
        // seat beside the browser it opens and the store that browser reads.
        // Turning that plugin off takes the command with it.
        CommandSpec::new(
            "doctor",
            "Show local sandbox/install diagnostics; not a full system doctor",
        )
        .zh_aliases(["诊断"])
        .surfaces(ALL_EVERYWHERE)
        .kind(Session),
        CommandSpec::new("hooks", "Show or reload hook configuration")
            .zh_aliases(["钩子"])
            .hint("[errors|reload]")
            .surfaces(ALL_EVERYWHERE)
            .kind(Session),
        CommandSpec::new("statusline", "Configure the status line").surfaces(Surfaces::TUI_ONLY),
        CommandSpec::new("fast", "Toggle the OpenAI fast service tier")
            .hint("[on|off|status]")
            .surfaces(Surfaces::TUI_ONLY),
        // `/login` and `/logout` are not here: the OAuth flow they begin and
        // end belongs to the onboarding plugin — the PKCE exchange, the
        // loopback listener and the credential write all live there — and
        // `/login` opens that plugin's wizard.
        CommandSpec::new("resume", "Resume a previous session")
            .surfaces(Surfaces::TUI_ONLY)
            .kind(Panel),
        CommandSpec::new("vim", "Toggle vim mode").surfaces(Surfaces::TUI_ONLY),
        CommandSpec::new("goal", "Set or show an auto-continuing persistent goal")
            .hint("[--max-sessions N] <objective>|status|clear|off|stop|archive")
            .surfaces(Surfaces::TUI_ONLY),
        CommandSpec::new("run", "Run a background shell task")
            .hint("<command>")
            .surfaces(Surfaces::TUI_ONLY),
        CommandSpec::new("exit", "Exit the application")
            .aliases(["quit"])
            .zh_aliases(["退出"])
            .surfaces(Surfaces::TUI_ONLY),
        // `/bg` used to mean this in the TUI and "open the task view" in the
        // app. The app's meaning won (see the crate docs): this one has a side
        // effect, and a user who types /bg expecting it and gets a list
        // notices at once, while the reverse silently moves their session.
        CommandSpec::new("background", "Move this session into Agent View")
            .category(Category::Agent)
            .surfaces(Surfaces::TUI_ONLY),
        // `/tasks`, `/workflows` and `/teams` are not here: the tasks plugin
        // registers them on the command seat beside the thirteen tools that
        // create the work they show. Turning that plugin off takes the commands
        // with it.
        // Its own entry rather than an alias of `/tasks`: in the terminal the
        // two open different overlays — this one the Agent View, `/tasks` the
        // background-task dialog — so folding them together would have
        // described one and run the other. The desktop has a single Agents
        // view and opens it for both names, which is what this command always
        // did there.
        CommandSpec::new("background-agents", "Open the background Agent View")
            .category(Category::Agent)
            .kind(Panel),
        CommandSpec::new(
            "kernel",
            "Run this session on rebon's native engine or an embedded kernel loop",
        )
        .hint("[rebon|dsh|pi|opencode|reload|list]")
        .surfaces(Surfaces::TUI_ONLY.with(CONTROL))
        .kind(Session),
        CommandSpec::new("hosted", "Detach this session into a background worker")
            .aliases(["host"])
            .surfaces(Surfaces::TUI_ONLY),
    ]
}

#[cfg(test)]
mod code_mode_tests {
    #[test]
    fn code_mode_is_a_shared_session_command() {
        let commands = super::builtin_command_table();
        let command = commands
            .iter()
            .find(|spec| spec.name == "codemode")
            .unwrap();
        for surface in [
            crate::Surface::Tui,
            crate::Surface::Web,
            crate::Surface::Mobile,
            crate::Surface::SessionControl,
        ] {
            assert!(command.available_on(surface));
        }
        assert_eq!(command.kind, crate::CommandKind::Session);
    }
}
