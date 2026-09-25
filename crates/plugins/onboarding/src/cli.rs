//! `rebon login` and `rebon logout`: account logins from a plain terminal.
//!
//! These are clap subcommands parsed before a kernel exists — the same
//! reason `rebon remote` lives in its plugin rather than on the command
//! seat: signing in has to work on a machine whose session cannot start
//! *because* it is not signed in. The flows are the ones the wizard drives
//! ([`crate::oauth`]); what is here is the line-by-line console around them.
//!
//! Everything prints to stdout and reads answers from stdin. Ctrl+C ends a
//! login that is waiting; nothing is written until the provider approves.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::AtomicBool;

use clap::Args;
use rebon_config::account_login::{AccountFlow, AccountLoginSpec};

use crate::accounts;
use crate::oauth::device::{self as oauth_device, ReqwestDeviceHttp};
use crate::oauth::{flow as oauth_flow, listener as oauth_listener};

/// `rebon login`.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct LoginArgs {
    /// The account to sign in to (`openai` for ChatGPT/Codex, `copilot`
    /// for GitHub Copilot). Asked for when omitted.
    #[arg(value_name = "ACCOUNT")]
    pub account: Option<String>,
    /// Show which accounts are signed in, and change nothing.
    #[arg(long, conflicts_with = "account")]
    pub status: bool,
}

/// `rebon logout`.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct LogoutArgs {
    /// The account to sign out of. May be omitted when only one is signed in.
    #[arg(value_name = "ACCOUNT")]
    pub account: Option<String>,
}

/// Run `rebon login`.
pub async fn run_login(args: LoginArgs) -> anyhow::Result<()> {
    let config_dir = rebon_config::config_home_dir();
    if args.status {
        for line in accounts::status_lines(&config_dir, rebon_types::wall_clock_ms())
            .map_err(anyhow::Error::msg)?
        {
            println!("{line}");
        }
        return Ok(());
    }
    let spec = match args.account.as_deref() {
        Some(name) => accounts::find_login(name).map_err(anyhow::Error::msg)?,
        None => pick_login()?,
    };
    let handle = tokio::runtime::Handle::current();
    // Every step below blocks (a loopback listener, stdin, the device poll),
    // so it runs off the async workers and bridges back with the handle.
    tokio::task::spawn_blocking(move || match spec.flow {
        AccountFlow::PkceLoopback { .. } => login_with_browser(&handle, spec),
        AccountFlow::DeviceCode { .. } => login_with_device_code(&handle, spec),
    })
    .await??;
    println!(
        "Signed in to {}. Provider `{}` is now active.",
        spec.display_name, spec.provider.name
    );
    Ok(())
}

/// Run `rebon logout`.
pub fn run_logout(args: LogoutArgs) -> anyhow::Result<()> {
    let config_dir = rebon_config::config_home_dir();
    let report =
        accounts::logout(&config_dir, args.account.as_deref()).map_err(anyhow::Error::msg)?;
    println!("{}", report.message);
    Ok(())
}

/// Ask which login to run, when stdin is a terminal to ask on.
fn pick_login() -> anyhow::Result<&'static AccountLoginSpec> {
    let logins = rebon_config::account_logins();
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "name the account to sign in to: rebon login <{}>",
            logins.iter().map(|s| s.id).collect::<Vec<_>>().join("|")
        );
    }
    println!("Sign in with:");
    for (index, spec) in logins.iter().enumerate() {
        println!(
            "  {}. {} — {}",
            index + 1,
            spec.picker_label,
            spec.description
        );
    }
    let answer = prompt_line(&format!("Choose 1-{}: ", logins.len()))?;
    choose_login(&answer)
}

/// Read a picker answer: a number from the list, or a login's name.
fn choose_login(answer: &str) -> anyhow::Result<&'static AccountLoginSpec> {
    let logins = rebon_config::account_logins();
    let answer = answer.trim();
    if let Ok(number) = answer.parse::<usize>() {
        return logins
            .get(number.wrapping_sub(1))
            .ok_or_else(|| anyhow::anyhow!("no choice {number}"));
    }
    accounts::find_login(answer).map_err(anyhow::Error::msg)
}

fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        anyhow::bail!("no answer (stdin closed)");
    }
    Ok(line)
}

/// The ChatGPT login: browser, loopback callback, paste fallback.
fn login_with_browser(
    handle: &tokio::runtime::Handle,
    spec: &AccountLoginSpec,
) -> anyhow::Result<()> {
    let challenge = oauth_flow::prepare_openai_oauth()?;
    println!("Signing in to {}.", spec.display_name);
    println!("Open this URL if the browser does not open by itself:");
    println!("{}", challenge.authorize_url);
    let _ = oauth_flow::launch_browser(&challenge.authorize_url);

    let callback = if challenge.port_available {
        println!("Waiting for the browser to come back (Ctrl+C to stop)…");
        match oauth_listener::wait_for_callback_with_cancel(
            &challenge.state,
            oauth_flow::CALLBACK_TIMEOUT,
            &std::sync::Arc::new(AtomicBool::new(false)),
        ) {
            Ok(callback) => callback,
            Err(err) => {
                println!("The automatic callback did not arrive ({err}).");
                paste_callback(&challenge.state)?
            }
        }
    } else {
        println!(
            "Port {} is busy, so the browser cannot hand the code back by itself.",
            oauth_listener::CALLBACK_PORT
        );
        paste_callback(&challenge.state)?
    };

    println!("Exchanging the code for a token…");
    handle.block_on(oauth_flow::exchange_and_persist(
        &callback.code,
        &challenge.verifier,
    ))?;
    Ok(())
}

fn paste_callback(state: &str) -> anyhow::Result<oauth_listener::CallbackResult> {
    loop {
        let pasted = prompt_line("Paste the full URL from the browser's address bar: ")?;
        match oauth_flow::parse_pasted_callback(&pasted, state) {
            Ok(callback) => return Ok(callback),
            Err(err) => println!("{err}"),
        }
    }
}

/// A device-code login: show the code, open the page, poll until approved.
fn login_with_device_code(
    handle: &tokio::runtime::Handle,
    spec: &'static AccountLoginSpec,
) -> anyhow::Result<()> {
    let http = ReqwestDeviceHttp::with_handle(handle.clone())?;
    let client_id =
        rebon_config::account_login::account_client_id(&rebon_config::config_home_dir(), spec);
    let authorization = oauth_device::request_device_authorization(&http, spec, &client_id)?;
    println!("Signing in to {}.", spec.display_name);
    println!(
        "Open {} and enter the code: {}",
        authorization.verification_uri, authorization.user_code
    );
    let _ = oauth_flow::launch_browser(authorization.page_to_open());
    println!("Waiting for approval (Ctrl+C to stop)…");
    let never = AtomicBool::new(false);
    let mut wait = oauth_device::sleep_unless_cancelled(&never);
    let tokens = oauth_device::poll_for_token(
        &http,
        spec,
        &client_id,
        &authorization,
        &mut wait,
        &rebon_types::wall_clock_ms,
    )?;
    println!("Approved; finishing sign-in…");
    handle.block_on(oauth_device::finish_device_login(spec, tokens))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picker_takes_a_number_or_a_name() {
        assert!(choose_login("1").unwrap().is_codex());
        assert_eq!(choose_login(" 2\n").unwrap().id, "copilot");
        assert_eq!(choose_login("github").unwrap().id, "copilot");
        assert!(choose_login("0").is_err());
        assert!(choose_login("9").is_err());
        assert!(choose_login("gemini").is_err());
    }

    #[derive(clap::Parser, Debug)]
    struct Harness {
        #[command(subcommand)]
        command: Sub,
    }

    #[derive(clap::Subcommand, Debug, PartialEq, Eq)]
    enum Sub {
        Login(LoginArgs),
        Logout(LogoutArgs),
    }

    #[test]
    fn login_and_logout_parse_their_arguments() {
        use clap::Parser;
        assert_eq!(
            Harness::parse_from(["rebon", "login", "copilot"]).command,
            Sub::Login(LoginArgs {
                account: Some("copilot".into()),
                status: false
            })
        );
        assert_eq!(
            Harness::parse_from(["rebon", "login", "--status"]).command,
            Sub::Login(LoginArgs {
                account: None,
                status: true
            })
        );
        assert!(Harness::try_parse_from(["rebon", "login", "copilot", "--status"]).is_err());
        assert_eq!(
            Harness::parse_from(["rebon", "logout"]).command,
            Sub::Logout(LogoutArgs { account: None })
        );
    }
}
