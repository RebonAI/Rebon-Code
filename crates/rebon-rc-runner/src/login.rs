//! `rebon rc login` and `rebon rc status`.

use std::collections::BTreeMap;

use anyhow::Context;
use rebon_bridge::devices::IssueDeviceRequest;
use rebon_bridge::http_client::{DeviceCredentials, HttpBridgeApiClient, HttpClientConfig};
use rebon_bridge::projects::ProjectInfo;
use rebon_session_host::BackgroundStore;
use serde::Serialize;

use crate::files::{RcDir, StoredCredentials};
use crate::ledger::{Ledger, SessionEntry};

/// What the token handed to `login` is.
#[derive(Clone)]
pub enum LoginCredential {
    /// The server's one-time bootstrap token: creates the account and this
    /// device.
    Bootstrap(String),
    /// Another device's access token: adds this device to its account.
    AccessToken(String),
    /// A refresh token minted elsewhere for this machine: adopted as is.
    RefreshToken(String),
}

/// Bind this machine to `server` and store the device refresh token.
pub async fn login(
    dir: &RcDir,
    server: &str,
    credential: LoginCredential,
    label: Option<String>,
) -> anyhow::Result<StoredCredentials> {
    let server = server.trim().trim_end_matches('/').to_string();
    let config = HttpClientConfig::new(server.clone());
    config
        .validate()
        .map_err(|message| anyhow::anyhow!("invalid server URL: {message}"))?;
    let stored = match credential {
        LoginCredential::Bootstrap(token) | LoginCredential::AccessToken(token) => {
            let issued = HttpBridgeApiClient::issue_device(
                &config,
                token.trim(),
                &IssueDeviceRequest { label },
            )
            .await
            .context("the RC server did not issue a device credential")?;
            StoredCredentials {
                server,
                account_id: issued.account_id,
                device_id: issued.device_id,
                refresh_token: issued.refresh_token,
                created_at_ms: rebon_types::wall_clock_ms(),
            }
        }
        LoginCredential::RefreshToken(token) => {
            let client = HttpBridgeApiClient::new(
                config,
                DeviceCredentials::new(String::new(), token.trim().to_string()),
            )?;
            client
                .refresh_now()
                .await
                .context("the RC server did not accept the refresh token")?;
            // A refresh token does not say whose it is; an empty account
            // means "unknown", which also retires any environment identity
            // this machine had.
            StoredCredentials {
                server,
                account_id: String::new(),
                device_id: String::new(),
                refresh_token: token.trim().to_string(),
                created_at_ms: rebon_types::wall_clock_ms(),
            }
        }
    };
    dir.save_credentials(&stored)?;
    Ok(stored)
}

/// The credentials `serve` needs, or the instruction to get them.
pub fn require_credentials(dir: &RcDir) -> anyhow::Result<StoredCredentials> {
    dir.load_credentials()?.context(
        "this machine is not bound to a Remote Control server; run `rebon rc login --server <url>`",
    )
}

/// What `rebon rc status` shows.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub logged_in: bool,
    pub server: Option<String>,
    pub account_id: Option<String>,
    pub device_id: Option<String>,
    pub environment_id: Option<String>,
    pub client_environment_id: Option<String>,
    pub serving_pid: Option<String>,
    pub projects: Vec<ProjectInfo>,
    pub projects_error: Option<String>,
    pub sessions: BTreeMap<String, SessionStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    #[serde(flatten)]
    pub entry: SessionEntry,
    /// The job's state on this machine, when it still has one.
    pub job_state: Option<String>,
}

pub fn status(
    dir: &RcDir,
    store: &BackgroundStore,
    projects: anyhow::Result<Vec<ProjectInfo>>,
) -> anyhow::Result<Status> {
    let credentials = dir.load_credentials()?;
    let environment = dir.load_environment()?;
    let sessions = Ledger::new(dir.clone())
        .sessions()?
        .into_iter()
        .map(|(rc_session, entry)| {
            let job_state = entry
                .job_id
                .as_deref()
                .and_then(|job_id| store.read_state(job_id).ok())
                .map(|job| job.status().as_str().to_string());
            (rc_session, SessionStatus { entry, job_state })
        })
        .collect();
    let (projects, projects_error) = match projects {
        Ok(projects) => (projects, None),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    let nonempty = |value: &str| Some(value.to_string()).filter(|value| !value.is_empty());
    Ok(Status {
        logged_in: credentials.is_some(),
        server: credentials.as_ref().map(|c| c.server.clone()),
        account_id: credentials.as_ref().and_then(|c| nonempty(&c.account_id)),
        device_id: credentials.as_ref().and_then(|c| nonempty(&c.device_id)),
        environment_id: environment.as_ref().and_then(|e| e.environment_id.clone()),
        client_environment_id: environment.map(|e| e.client_environment_id),
        serving_pid: dir.serve_holder(),
        projects,
        projects_error,
        sessions,
    })
}

/// The human form of [`Status`].
pub fn render_status(status: &Status) -> String {
    let mut out = String::new();
    let unknown = || "unknown".to_string();
    match &status.server {
        Some(server) => {
            out.push_str(&format!("server       {server}\n"));
            out.push_str(&format!(
                "device       {} (account {})\n",
                status.device_id.clone().unwrap_or_else(unknown),
                status.account_id.clone().unwrap_or_else(unknown)
            ));
        }
        None => out.push_str("server       not logged in (rebon rc login --server <url>)\n"),
    }
    out.push_str(&format!(
        "environment  {}\n",
        status
            .environment_id
            .clone()
            .unwrap_or_else(|| "not registered yet".into())
    ));
    out.push_str(&format!(
        "serve        {}\n",
        status
            .serving_pid
            .as_ref()
            .map(|pid| format!("running (pid {pid})"))
            .unwrap_or_else(|| "not running".into())
    ));
    match &status.projects_error {
        Some(error) => out.push_str(&format!("projects     cannot be resolved: {error}\n")),
        None => {
            out.push_str("projects\n");
            for project in &status.projects {
                out.push_str(&format!("  {}  ({})\n", project.path, project.label));
            }
        }
    }
    if status.sessions.is_empty() {
        out.push_str("sessions     none yet\n");
    } else {
        out.push_str("sessions\n");
        for (rc_session, session) in &status.sessions {
            out.push_str(&format!(
                "  {rc_session} -> {} in {}{}\n",
                session.entry.rebon_session_id,
                session.entry.cwd,
                session
                    .job_state
                    .as_ref()
                    .map(|state| format!(" [{state}]"))
                    .unwrap_or_default()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::EnvironmentIdentity;

    #[tokio::test]
    async fn a_bad_server_url_is_refused_before_anything_is_stored() {
        let home = tempfile::tempdir().unwrap();
        let dir = RcDir::new(home.path());
        let error = login(
            &dir,
            "rc.example.com",
            LoginCredential::Bootstrap("t".into()),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("invalid server URL"), "{error}");
        assert!(dir.load_credentials().unwrap().is_none());
        assert!(require_credentials(&dir)
            .unwrap_err()
            .to_string()
            .contains("rebon rc login"));
    }

    #[test]
    fn status_reports_what_is_on_disk_and_never_a_token() {
        let home = tempfile::tempdir().unwrap();
        let dir = RcDir::new(home.path());
        let store = BackgroundStore::new(home.path().join("jobs"));
        let empty = status(&dir, &store, Ok(Vec::new())).unwrap();
        assert!(!empty.logged_in);
        assert!(render_status(&empty).contains("not logged in"));
        assert!(render_status(&empty).contains("sessions     none yet"));

        dir.save_credentials(&StoredCredentials {
            server: "https://rc.example.com".into(),
            account_id: "acct_1".into(),
            device_id: "dev_1".into(),
            refresh_token: "refresh-secret".into(),
            created_at_ms: 1,
        })
        .unwrap();
        let mut identity = EnvironmentIdentity::fresh("https://rc.example.com");
        identity.environment_id = Some("env_1".into());
        dir.save_environment(&identity).unwrap();
        Ledger::new(dir.clone())
            .record_session(
                "sess_1",
                SessionEntry {
                    rebon_session_id: "local-1".into(),
                    project: "/srv/app".into(),
                    cwd: "/srv/app".into(),
                    job_id: Some("bg-missing".into()),
                    environment_id: "env_1".into(),
                    updated_at_ms: 1,
                },
            )
            .unwrap();
        let full = status(&dir, &store, Ok(vec![ProjectInfo::new("/srv/app", "App")])).unwrap();
        assert!(full.logged_in);
        assert_eq!(full.environment_id.as_deref(), Some("env_1"));
        assert_eq!(full.sessions["sess_1"].job_state, None);
        let text = render_status(&full);
        assert!(text.contains("https://rc.example.com"));
        assert!(text.contains("dev_1 (account acct_1)"));
        assert!(text.contains("env_1"));
        assert!(text.contains("/srv/app  (App)"));
        assert!(text.contains("sess_1 -> local-1 in /srv/app"));
        assert!(text.contains("not running"));
        let json = serde_json::to_string(&full).unwrap();
        assert!(!json.contains("refresh-secret"));
        assert!(!text.contains("refresh-secret"));

        let broken = status(&dir, &store, Err(anyhow::anyhow!("no such dir"))).unwrap();
        assert!(render_status(&broken).contains("cannot be resolved: no such dir"));
    }
}
