//! The dispatcher every surface shares.
//!
//! A name and its arguments come in -- typed in a terminal, forwarded
//! over IPC by the desktop app, or translated from a typed request by
//! `serve` -- and one of the session commands answers. The catalog's
//! `SESSION_CONTROL` surface bit is the set that may be forwarded by name;
//! `/rewind` answers here but deliberately does not carry it.

use rebon_slash_commands::{find, Surface};

use super::context::{
    execute_compact_command, execute_context_command, execute_memory_command,
    execute_prune_command, parse_compact_command, parse_prune_command,
};
use super::cost::execute_cost_command;
use super::doctor::execute_doctor_command;
use super::hooks::{execute_hooks_command, parse_hooks_command};
use super::mcp::{execute_mcp_command, parse_mcp_command};
use super::permissions::{execute_permissions_command, parse_permissions_command};
use super::{SessionCommandInputs, SessionControlCommandResult};
use crate::EngineSession;

/// Split `/name arg…` into its parts, but only for a command that must
/// execute where the engine actually lives.
///
/// Those read or mutate real session state — the context manager, the
/// transcript, the denial store, the running kernel — so a UI process that
/// is merely mirroring a worker cannot answer them from its own shell
/// session. Three call sites share this gate: the worker's IPC server
/// (`execute_session_control_command` below), the desktop app, and the TUI's
/// forwarding gate for a RemoteProxy attachment. `None` for everything else
/// — local UI commands (`/vim`, `/help`, `/theme`) belong to whichever
/// process is drawing the screen and must not be forwarded anywhere.
///
/// Which commands those are is the catalog's `SESSION_CONTROL` surface bit,
/// not a list here. Twelve names used to be written out, and the bit was
/// added to replace them and the list stayed — so a command carrying the bit
/// and missing from the list either ran against the wrong process or, the way
/// `/backend` first arrived, reached no process at all and became prompt
/// text. `the_match_answers_nothing_the_list_omits` still pins the bit and
/// the match to each other in both directions.
///
/// The name that comes back is the catalog's, not the spelling typed: the
/// dispatcher below matches canonical names, so forwarding an alias would
/// reach the owner as a command it does not answer.
pub fn parse_session_control_command(text: &str) -> Option<(String, Vec<String>)> {
    let mut parts = text.trim().strip_prefix('/')?.split_whitespace();
    let spec = find(parts.next()?)?;
    spec.available_on(Surface::SessionControl)
        .then(|| (spec.name.to_string(), parts.map(str::to_string).collect()))
}

pub fn execute_session_control_command(
    inputs: &SessionCommandInputs,
    session: &EngineSession,
    name: &str,
    args: &[String],
) -> Result<SessionControlCommandResult, String> {
    let name = name.trim().trim_start_matches('/').to_ascii_lowercase();
    let command_text = if args.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {}", args.join(" "))
    };
    let result = match name.as_str() {
        "context" if args.is_empty() => SessionControlCommandResult {
            output: control_command_output(execute_context_command(inputs, session), false),
            replay_requests: Vec::new(),
        },
        "memory" if args.is_empty() => SessionControlCommandResult {
            output: control_command_output(execute_memory_command(session), false),
            replay_requests: Vec::new(),
        },
        "doctor" if args.is_empty() => SessionControlCommandResult {
            output: control_command_output(execute_doctor_command(inputs, session), false),
            replay_requests: Vec::new(),
        },
        "status" if args.is_empty() => SessionControlCommandResult {
            output: control_command_output(
                super::status::execute_status_command(inputs, session),
                false,
            ),
            replay_requests: Vec::new(),
        },
        "cost" if args.is_empty() => SessionControlCommandResult {
            output: control_command_output(execute_cost_command(session, inputs), false),
            replay_requests: Vec::new(),
        },
        "mcp" => {
            let command = parse_mcp_command(&command_text)
                .ok_or_else(|| "invalid /mcp arguments".to_string())?
                .map_err(|usage| usage)?;
            let output = execute_mcp_command(session, command);
            let is_err = output.starts_with('!');
            SessionControlCommandResult {
                output: control_command_output(output, is_err),
                replay_requests: Vec::new(),
            }
        }
        "hooks" => {
            let command = parse_hooks_command(&command_text)
                .ok_or_else(|| "invalid /hooks arguments".to_string())?;
            SessionControlCommandResult {
                output: control_command_output(execute_hooks_command(session, command), false),
                replay_requests: Vec::new(),
            }
        }
        "compact" => {
            let command = parse_compact_command(&command_text)
                .ok_or_else(|| "invalid /compact arguments".to_string())?;
            SessionControlCommandResult {
                output: control_command_output(
                    execute_compact_command(session, command.instructions.as_deref()),
                    false,
                ),
                replay_requests: Vec::new(),
            }
        }
        "prune" => {
            let command = parse_prune_command(&command_text)
                .ok_or_else(|| "invalid /prune arguments".to_string())?;
            SessionControlCommandResult {
                output: control_command_output(execute_prune_command(command, session), false),
                replay_requests: Vec::new(),
            }
        }
        "permissions" => {
            let command = parse_permissions_command(&command_text)
                .ok_or_else(|| "invalid /permissions arguments".to_string())?;
            let result = execute_permissions_command(inputs, command);
            SessionControlCommandResult {
                output: control_command_output(result.text.clone(), result.text.starts_with('!')),
                replay_requests: result.replay_requests,
            }
        }
        // Switching which agent runs the session, from any front end that can
        // send a session command — which the desktop app has and used to lack
        // a use for: its `/agent` was a Prompt, so the text went to the model
        // and the session kept running on whatever it was already running on.
        //
        // `/backend` only. A terminal's `/agent` reaches the same switch, but
        // it also means "spawn a sub-agent with this prompt", and that meaning
        // belongs to the terminal that typed it — answering to the name here
        // would let a front end send a prompt down this channel and silently
        // switch the session with it.
        "backend" => {
            let args_text = args.join(" ");
            let result = rebon_agent_core::routing::handle_agent_command(
                &session.engine_half.runtime.session_agents,
                &args_text,
                session.runtime_handle().as_ref(),
            );
            SessionControlCommandResult {
                output: control_command_output(result.text, result.is_err),
                replay_requests: Vec::new(),
            }
        }
        // Which kernel runs this session — the desktop app's (and
        // mobile's) face for the TUI's /kernel: switch_to persists the
        // choice through the session sidecar, so it survives worker
        // respawns.
        "kernel" => {
            let args_text = args.join(" ");
            let result = crate::commands::kernel::handle_kernel_command(
                &session.engine_half.runtime.session_agents,
                &args_text,
                session.runtime_handle().as_ref(),
            );
            SessionControlCommandResult {
                output: control_command_output(result.text, result.is_err),
                replay_requests: Vec::new(),
            }
        }
        // Deliberately absent from `SESSION_CONTROL_COMMANDS`: a terminal's
        // own `/rewind` opens a picker, and forwarding the typed word would
        // replace that with a command nobody composed. This arm exists for the
        // typed `Rewind` request an owner receives, which arrives with the
        // turn already chosen.
        "rewind" => {
            let mut args = args.iter();
            let uuid = args
                .next()
                .ok_or_else(|| "rewind needs the turn to rewind to".to_string())?;
            let scope = match args.next().map(String::as_str) {
                None | Some("conversation") => rebon_session_host::RewindScopeWire::Conversation,
                Some("code") => rebon_session_host::RewindScopeWire::Code,
                Some("both") => rebon_session_host::RewindScopeWire::Both,
                Some(other) => return Err(format!("unknown rewind scope `{other}`")),
            };
            let report = super::rewind::rewind_session_for_owner(inputs, session, uuid, scope)?;
            SessionControlCommandResult {
                output: control_command_output(report.describe(), report.errors > 0),
                replay_requests: Vec::new(),
            }
        }
        "context" | "memory" | "doctor" | "status" | "cost" => {
            return Err(format!("/{name} does not accept arguments"));
        }
        _ => return Err(format!("unsupported session command: /{name}")),
    };
    Ok(result)
}

fn control_command_output(text: String, warning: bool) -> rebon_session_host::CommandOutput {
    rebon_session_host::CommandOutput {
        text,
        tone: if warning { "warning" } else { "info" }.into(),
    }
}
