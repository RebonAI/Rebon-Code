//! The OAuth token refresher an OpenAI Responses provider is built with.
//!
//! Sits here rather than in the assembly layer because building a provider
//! client is what needs it, and the registry that builds one is
//! below the harness. Nothing about it is assembly: it reads
//! `.credentials.json`, decides whether the access token on disk is still
//! good, and otherwise drives `rebon_config::force_refresh_openai_token`.

use rebon_api::TokenRefresher;
use rebon_config::{force_refresh_openai_token, is_token_expired};
#[derive(Debug, Clone, PartialEq, Eq)]
enum DiskOAuthRefreshDecision {
    FreshAccess {
        access_token: String,
        refresh_token: Option<String>,
    },
    RefreshWith(String),
    None,
}

fn non_empty_string(value: Option<&String>) -> Option<String> {
    value.filter(|value| !value.is_empty()).cloned()
}

fn choose_disk_oauth_refresh_decision(
    tokens: Option<&rebon_config::OpenAIOAuthTokens>,
) -> DiskOAuthRefreshDecision {
    let Some(tokens) = tokens else {
        return DiskOAuthRefreshDecision::None;
    };

    let disk_refresh = non_empty_string(tokens.refresh_token.as_ref());
    if !tokens.access_token.is_empty() && !is_token_expired(tokens.expires_at) {
        return DiskOAuthRefreshDecision::FreshAccess {
            access_token: tokens.access_token.clone(),
            refresh_token: disk_refresh,
        };
    }

    disk_refresh
        .map(DiskOAuthRefreshDecision::RefreshWith)
        .unwrap_or(DiskOAuthRefreshDecision::None)
}

/// [`TokenRefresher`] implementation that drives
/// [`rebon_config::force_refresh_openai_token`] when the
/// provider reports a 401.
///
/// Holds the current refresh token in a `std::sync::Mutex` so the
/// refresh path can rotate it when the server issues a new one.
/// Dropping the provider drops the refresher too, which is fine —
/// the next rebon invocation re-reads the rotated tokens from
/// `.credentials.json`.
#[derive(Debug)]
pub struct RebonOAuthRefresher {
    config_dir: std::path::PathBuf,
    pub(crate) refresh_token: std::sync::Mutex<Option<String>>,
}

impl RebonOAuthRefresher {
    /// Build a refresher for the given config dir, seeded with
    /// the initial refresh token (pulled from `.credentials.json`
    /// during provider resolution).
    pub fn new(config_dir: std::path::PathBuf, initial_refresh_token: Option<String>) -> Self {
        Self {
            config_dir,
            refresh_token: std::sync::Mutex::new(initial_refresh_token),
        }
    }
}

#[async_trait::async_trait]
impl TokenRefresher for RebonOAuthRefresher {
    async fn refresh(&self) -> Result<String, String> {
        let disk_decision = rebon_config::read_credentials(&self.config_dir)
            .map(|credentials| {
                choose_disk_oauth_refresh_decision(credentials.openai_oauth.as_ref())
            })
            .map_err(|err| format!("{err}"))?;

        let current_refresh = match disk_decision {
            DiskOAuthRefreshDecision::FreshAccess {
                access_token,
                refresh_token,
            } => {
                if let Some(new_refresh) = refresh_token {
                    {
                        let mut guard = self
                            .refresh_token
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *guard = Some(new_refresh);
                    }
                }
                return Ok(access_token);
            }
            DiskOAuthRefreshDecision::RefreshWith(refresh_token) => {
                {
                    let mut guard = self
                        .refresh_token
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *guard = Some(refresh_token.clone());
                }
                refresh_token
            }
            DiskOAuthRefreshDecision::None => {
                let guard = self
                    .refresh_token
                    .lock()
                    .map_err(|err| format!("refresh token mutex poisoned: {err}"))?;
                guard.clone().ok_or_else(|| {
                    "no refresh token stored — run `rebon` and `/login`".to_string()
                })?
            }
        };

        let outcome = force_refresh_openai_token(&self.config_dir, &current_refresh)
            .await
            .map_err(|err| format!("{err}"))?;
        // Rotate the in-memory refresh token too so subsequent
        // refreshes (if any) use the newly-minted one.
        if let Some(new_refresh) = outcome.tokens.refresh_token.clone() {
            {
                let mut guard = self
                    .refresh_token
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = Some(new_refresh);
            }
        }
        Ok(outcome.tokens.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rebon_oauth_refresher_uses_fresh_disk_access_token_and_updates_refresh_token() {
        let temp = tempfile::tempdir().expect("temp dir");
        rebon_config::write_openai_oauth_tokens(
            temp.path(),
            &rebon_config::OpenAIOAuthTokens {
                access_token: "disk-access-token".to_string(),
                refresh_token: Some("disk-refresh-token".to_string()),
                expires_at: Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("system clock after unix epoch")
                        .as_millis() as u64
                        + rebon_config::TOKEN_EXPIRY_BUFFER_MS
                        + 60_000,
                ),
            },
        )
        .expect("write oauth tokens");

        let refresher = RebonOAuthRefresher::new(
            temp.path().to_path_buf(),
            Some("old-refresh-token".to_string()),
        );

        let access_token = refresher.refresh().await.expect("refresh succeeds");
        assert_eq!(access_token, "disk-access-token");
        assert_eq!(
            *refresher.refresh_token.lock().expect("refresh token mutex"),
            Some("disk-refresh-token".to_string())
        );
    }

    #[test]
    fn rebon_oauth_refresher_selects_disk_refresh_token_when_disk_access_expired() {
        let tokens = rebon_config::OpenAIOAuthTokens {
            access_token: "expired-disk-access-token".to_string(),
            refresh_token: Some("newer-disk-refresh-token".to_string()),
            expires_at: Some(0),
        };

        assert_eq!(
            choose_disk_oauth_refresh_decision(Some(&tokens)),
            DiskOAuthRefreshDecision::RefreshWith("newer-disk-refresh-token".to_string())
        );
    }
}
