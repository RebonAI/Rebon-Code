//! Account logins: signing in with a subscription instead of pasting a key.
//!
//! Each row of [`ACCOUNT_LOGINS`] is everything that differs between two
//! logins — how the user proves who they are, which endpoints and client id
//! that takes, what a model request carries as its bearer, which headers the
//! API insists on, and the provider entry a successful login leaves behind.
//! Everything that is the same for all of them lives once, beside the table:
//! where the tokens are stored, how an entry names its login (the sentinel
//! `apiKey`), how a stored login becomes a bearer at resolution time, how an
//! expired bearer is renewed, and how a login is removed again.
//!
//! ## Which logins are here
//!
//! Only flows the provider sanctions for third-party clients, or that a wide
//! set of editors already use with a client id the provider published in its
//! own open-source client. Every constant cites where it was read. Claude
//! subscription logins are deliberately absent (Anthropic's terms forbid
//! third-party use), and so is the Gemini CLI login (Google's terms for that
//! client forbid it). The Qwen device login was shut down on 2026-04-15 and
//! xAI's device login belongs to a closed-source client; both vendors are
//! reached with an API key instead (see `PROVIDER_PRESETS`).
//!
//! ## Storage
//!
//! Tokens live in `.credentials.json`. The ChatGPT login keeps its original
//! `openaiOAuth` key rather than being migrated: the desktop app and a
//! globally installed `rebon` can be different versions reading the same
//! file, and an older binary only knows that key. Every other login is an
//! entry under `oauthAccounts`, keyed by its id. Unknown keys — at the top
//! level and inside each entry — survive every rewrite.
//!
//! ## The sentinel
//!
//! A provider entry signed in through a login stores a sentinel instead of a
//! secret: `$OAUTH:<id>`, or, for the ChatGPT login, the `$OPENAI_OAUTH_TOKEN`
//! it has always used. Resolution swaps the sentinel for a live bearer and
//! attaches [`OAuthMeta`] naming the login, which is what lets the capability
//! gates that belong to the ChatGPT login ask "is this *that* login" rather
//! than "is this any login".

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    force_refresh_openai_token, merge_discovered_models, read_config_roundtrip, read_credentials,
    upsert_openai_oauth_provider_in, wire_family_for_format, write_config_roundtrip,
    write_credentials, write_openai_oauth_tokens, Credentials, CustomProvider,
    CustomProviderModels, ModelProfileMap, OAuthMeta, OAuthRefreshError, OAuthRefreshErrorKind,
    OpenAIOAuthTokens, ProviderOptions, OAUTH_REFRESH_TIMEOUT_MS, OPENAI_OAUTH_CLIENT_ID,
    OPENAI_OAUTH_PROVIDER_BASE_URL, OPENAI_OAUTH_PROVIDER_MODEL, OPENAI_OAUTH_PROVIDER_MODELS,
    OPENAI_OAUTH_PROVIDER_NAME, OPENAI_OAUTH_TOKEN_SENTINEL, OPENAI_OAUTH_TOKEN_URL,
};

/// How the user proves who they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountFlow {
    /// Authorization code with PKCE (RFC 7636), the redirect caught on a
    /// loopback port (RFC 8252 §7.3), with a paste fallback when the port is
    /// taken.
    PkceLoopback {
        authorize_url: &'static str,
        redirect_uri: &'static str,
        redirect_port: u16,
    },
    /// Device authorization grant (RFC 8628): the user types a short code on
    /// a page the provider hosts while this side polls the token endpoint.
    DeviceCode {
        device_authorization_url: &'static str,
    },
}

/// What a model request carries as its bearer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestCredential {
    /// The OAuth access token itself, renewed with its refresh token.
    AccessToken,
    /// A short-lived session token minted from the OAuth token with
    /// `GET exchange_url` and `Authorization: token <oauth token>`. The OAuth
    /// token does not expire; renewing is exchanging again.
    ExchangedSession { exchange_url: &'static str },
}

/// Where a login's tokens live in `.credentials.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSlot {
    /// The top-level `openaiOAuth` key the ChatGPT login has always written.
    OpenAiOAuth,
    /// `oauthAccounts.<id>`.
    Accounts,
}

/// The `customProviders[]` entry a successful login upserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountProviderEntry {
    pub name: &'static str,
    /// `openai`, `openai-responses` or `anthropic`.
    pub format: &'static str,
    /// The entry's `vendor` pin, when the host alone would not say.
    pub vendor: Option<&'static str>,
    /// Where requests go until the login reports an endpoint of its own.
    pub base_url: &'static str,
    pub default_model: &'static str,
    /// Seeded into the entry on login; a listing, where the API has one,
    /// adds the rest.
    pub models: &'static [&'static str],
}

/// One way to sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountLoginSpec {
    /// Stable id: the `rebon login <id>` argument, the `$OAUTH:<id>`
    /// sentinel and the `oauthAccounts` key.
    pub id: &'static str,
    /// Other names `rebon login` accepts.
    pub aliases: &'static [&'static str],
    /// The service, as a notice names it ("Your … session has expired").
    pub display_name: &'static str,
    /// The picker row's first line; also "<label> connected." on success.
    pub picker_label: &'static str,
    /// The picker row's second line.
    pub description: &'static str,
    /// The heading the picker files the row under.
    pub group: &'static str,
    pub flow: AccountFlow,
    pub token_url: &'static str,
    pub client_id: &'static str,
    /// Space-separated, as the authorization request sends them.
    pub scopes: &'static str,
    pub request_credential: RequestCredential,
    /// Headers every model request (and model listing) must carry. A header
    /// the entry's own `options.headers` sets wins.
    pub request_headers: &'static [(&'static str, &'static str)],
    /// The `apiKey` value an entry stores to name this login.
    pub sentinel: &'static str,
    pub storage: CredentialSlot,
    pub provider: AccountProviderEntry,
    /// Where the endpoints and client id were read, for a reviewer.
    pub provenance: &'static str,
}

/// The id of the ChatGPT (Codex) login — the one login several features
/// that talk to OpenAI beyond the model wire are gated on.
pub const CODEX_LOGIN_ID: &str = "openai";

/// The id of the GitHub Copilot login.
pub const COPILOT_LOGIN_ID: &str = "copilot";

/// Prefix of the generic sentinel: `$OAUTH:<id>`.
pub const ACCOUNT_SENTINEL_PREFIX: &str = "$OAUTH:";

/// How Rebon names itself to an API that asks which editor is calling. It
/// never claims to be another client.
const REBON_EDITOR_VERSION: &str = concat!("Rebon/", env!("CARGO_PKG_VERSION"));
const REBON_PLUGIN_VERSION: &str = concat!("rebon/", env!("CARGO_PKG_VERSION"));

/// Every login, in picker order.
pub const ACCOUNT_LOGINS: &[AccountLoginSpec] = &[
    AccountLoginSpec {
        id: CODEX_LOGIN_ID,
        aliases: &["codex", "chatgpt"],
        display_name: "ChatGPT (Codex)",
        picker_label: "OpenAI account",
        description: "ChatGPT Plus or Pro subscription (via Codex OAuth)",
        group: "OpenAI",
        flow: AccountFlow::PkceLoopback {
            authorize_url: "https://auth.openai.com/oauth/authorize",
            redirect_uri: "http://localhost:1455/auth/callback",
            redirect_port: 1455,
        },
        token_url: OPENAI_OAUTH_TOKEN_URL,
        client_id: OPENAI_OAUTH_CLIENT_ID,
        scopes: "openid profile email offline_access",
        request_credential: RequestCredential::AccessToken,
        request_headers: &[],
        sentinel: OPENAI_OAUTH_TOKEN_SENTINEL,
        storage: CredentialSlot::OpenAiOAuth,
        provider: AccountProviderEntry {
            name: OPENAI_OAUTH_PROVIDER_NAME,
            format: "openai-responses",
            vendor: None,
            base_url: OPENAI_OAUTH_PROVIDER_BASE_URL,
            default_model: OPENAI_OAUTH_PROVIDER_MODEL,
            models: OPENAI_OAUTH_PROVIDER_MODELS,
        },
        provenance: "OpenAI Codex CLI (github.com/openai/codex, Apache-2.0), codex-rs/login",
    },
    AccountLoginSpec {
        id: COPILOT_LOGIN_ID,
        aliases: &["github-copilot", "github"],
        display_name: "GitHub Copilot",
        picker_label: "GitHub Copilot account",
        description: "Copilot Individual, Business or Enterprise (device login)",
        group: "GitHub",
        // docs.github.com "Authorizing OAuth apps" › device flow.
        flow: AccountFlow::DeviceCode {
            device_authorization_url: "https://github.com/login/device/code",
        },
        token_url: "https://github.com/login/oauth/access_token",
        // GitHub's own Copilot client id, public in the MIT-licensed
        // github/copilot.vim (copilot-language-server/dist) and used by
        // copilot.lua, LiteLLM and the other Copilot integrations.
        client_id: "Iv1.b507a08c87ecfe98",
        scopes: "read:user",
        request_credential: RequestCredential::ExchangedSession {
            exchange_url: "https://api.github.com/copilot_internal/v2/token",
        },
        // Copilot rejects a request that does not name its editor. Rebon
        // names itself; `Copilot-Integration-Id` is left out because the
        // values the language server knows are reserved for GitHub's own
        // editors.
        request_headers: &[
            ("Editor-Version", REBON_EDITOR_VERSION),
            ("Editor-Plugin-Version", REBON_PLUGIN_VERSION),
            ("Openai-Intent", "conversation-edits"),
            ("X-GitHub-Api-Version", "2025-10-01"),
        ],
        sentinel: "$OAUTH:copilot",
        storage: CredentialSlot::Accounts,
        provider: AccountProviderEntry {
            name: "copilot",
            format: "openai",
            vendor: Some("github-copilot"),
            base_url: "https://api.githubcopilot.com",
            // models.dev providers/github-copilot: the model every plan
            // includes first, then flagships a paid plan adds.
            default_model: "gpt-5-mini",
            models: &[
                "gpt-5-mini",
                "gpt-5.6-sol",
                "claude-sonnet-5",
                "gemini-3.7-flash",
                "grok-4.7",
            ],
        },
        provenance: "client id from github/copilot.vim (MIT); device flow per \
                     docs.github.com; token exchange and headers as \
                     zed-industries/zed crates/copilot and BerriAI/litellm \
                     llms/github_copilot call them",
    },
];

impl AccountLoginSpec {
    /// Whether this is the ChatGPT (Codex) login.
    pub fn is_codex(&self) -> bool {
        self.id == CODEX_LOGIN_ID
    }

    /// Whether `name` names this login: its id or an alias, any case.
    pub fn answers_to(&self, name: &str) -> bool {
        let name = name.trim();
        self.id.eq_ignore_ascii_case(name)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    }
}

/// The ChatGPT (Codex) login's row.
pub fn codex_login() -> &'static AccountLoginSpec {
    account_login(CODEX_LOGIN_ID).expect("the table has a ChatGPT row")
}

/// Every login, in picker order.
pub fn account_logins() -> &'static [AccountLoginSpec] {
    ACCOUNT_LOGINS
}

/// The login `name` names (id or alias, any case).
pub fn account_login(name: &str) -> Option<&'static AccountLoginSpec> {
    ACCOUNT_LOGINS.iter().find(|spec| spec.answers_to(name))
}

/// The login a stored `apiKey` names, when it is a sentinel. Exact match,
/// as resolution has always compared it: a key with stray whitespace is a
/// literal key, not a login.
pub fn account_login_for_api_key(api_key: &str) -> Option<&'static AccountLoginSpec> {
    if let Some(id) = api_key.strip_prefix(ACCOUNT_SENTINEL_PREFIX) {
        return ACCOUNT_LOGINS.iter().find(|spec| spec.id == id);
    }
    ACCOUNT_LOGINS.iter().find(|spec| spec.sentinel == api_key)
}

/// Whether `api_key` claims to be a login sentinel, known or not. An
/// unknown `$OAUTH:<id>` must fail resolution rather than be sent to the
/// endpoint as if it were a key.
pub fn is_account_sentinel(api_key: &str) -> bool {
    api_key.starts_with(ACCOUNT_SENTINEL_PREFIX) || account_login_for_api_key(api_key).is_some()
}

/// Longest client id [`account_client_id`] accepts from `config.json`.
/// GitHub's are 20 characters and OAuth app ids elsewhere stay well under
/// 100; anything longer is a pasted secret or a mistake.
const MAX_CLIENT_ID_LEN: usize = 128;

/// The client id a device-code login sends: the table's, unless
/// `config.json` names another under `accountLogins.<id>.clientId` — for an
/// organisation that registered its own OAuth app, or for when the published
/// id stops being honoured.
///
/// The ChatGPT login always answers the table's: its client id is registered
/// together with the loopback redirect the flow listens on, so another id
/// could not complete it. A value that is not a short run of visible ASCII
/// is ignored rather than sent.
pub fn account_client_id(config_dir: &Path, spec: &AccountLoginSpec) -> String {
    if spec.is_codex() {
        return spec.client_id.to_string();
    }
    let configured = read_config_roundtrip(config_dir).ok().and_then(|config| {
        config
            .extra
            .get("accountLogins")?
            .get(spec.id)?
            .get("clientId")?
            .as_str()
            .map(|id| id.trim().to_string())
    });
    match configured {
        Some(id) if is_plausible_client_id(&id) => id,
        Some(_) => {
            tracing::warn!(
                login = spec.id,
                "rebon: ignoring accountLogins.{}.clientId: not a client id",
                spec.id
            );
            spec.client_id.to_string()
        }
        None => spec.client_id.to_string(),
    }
}

fn is_plausible_client_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_CLIENT_ID_LEN && id.bytes().all(|b| b.is_ascii_graphic())
}

/// One login's tokens, as stored under `oauthAccounts.<id>` (and, for the
/// ChatGPT login, projected from `openaiOAuth`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTokens {
    /// The OAuth access token.
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Access-token expiry, ms since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The exchanged request token ([`RequestCredential::ExchangedSession`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// Session-token expiry, ms since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_expires_at: Option<u64>,
    /// The API endpoint the exchange named for this account (Copilot plans
    /// each have their own host).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// Keys a later version wrote, kept on rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl AccountTokens {
    /// A fresh login's tokens: nothing exchanged yet.
    pub fn new(
        access_token: String,
        refresh_token: Option<String>,
        expires_at: Option<u64>,
    ) -> Self {
        Self {
            access_token,
            refresh_token,
            expires_at,
            ..Self::default()
        }
    }
}

/// The login's tokens inside an already-read credentials file.
pub fn account_tokens_in(
    credentials: &Credentials,
    spec: &AccountLoginSpec,
) -> Option<AccountTokens> {
    match spec.storage {
        CredentialSlot::OpenAiOAuth => credentials.openai_oauth.as_ref().map(|tokens| {
            AccountTokens::new(
                tokens.access_token.clone(),
                tokens.refresh_token.clone(),
                tokens.expires_at,
            )
        }),
        CredentialSlot::Accounts => credentials.accounts.get(spec.id).cloned(),
    }
}

/// The login's tokens on disk, or `None` when it is not signed in.
pub fn read_account_tokens(
    config_dir: &Path,
    spec: &AccountLoginSpec,
) -> anyhow::Result<Option<AccountTokens>> {
    Ok(account_tokens_in(&read_credentials(config_dir)?, spec))
}

/// Store the login's tokens, keeping every other key in the file.
pub fn write_account_tokens(
    config_dir: &Path,
    spec: &AccountLoginSpec,
    tokens: &AccountTokens,
) -> anyhow::Result<()> {
    match spec.storage {
        CredentialSlot::OpenAiOAuth => write_openai_oauth_tokens(
            config_dir,
            &OpenAIOAuthTokens {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires_at: tokens.expires_at,
            },
        ),
        CredentialSlot::Accounts => {
            let mut credentials = read_credentials(config_dir)?;
            credentials
                .accounts
                .insert(spec.id.to_string(), tokens.clone());
            write_credentials(config_dir, &credentials)
        }
    }
}

/// Remove the login's tokens and nothing else. Returns whether there was
/// anything to remove; when there was not, the file is not rewritten.
///
/// The provider entry is left in place: it holds the user's model choices,
/// and resolving it afterwards fails with the instruction to sign in again.
pub fn sign_out_account(config_dir: &Path, spec: &AccountLoginSpec) -> anyhow::Result<bool> {
    let mut credentials = read_credentials(config_dir)?;
    let removed = match spec.storage {
        CredentialSlot::OpenAiOAuth => credentials.openai_oauth.take().is_some(),
        CredentialSlot::Accounts => credentials.accounts.remove(spec.id).is_some(),
    };
    if removed {
        write_credentials(config_dir, &credentials)?;
    }
    Ok(removed)
}

/// The hint every "not signed in" message ends with.
fn sign_in_hint(spec: &AccountLoginSpec) -> String {
    format!("run `rebon login {}` or /login", spec.id)
}

/// Turn a stored login into the bearer a request carries, plus the
/// [`OAuthMeta`] the refresh paths need. For every login but the ChatGPT one,
/// whose resolution keeps its original wording in `resolve_api_key`.
pub(crate) fn resolve_account_bearer(
    spec: &'static AccountLoginSpec,
    credentials: &Credentials,
    provider_name: &str,
) -> anyhow::Result<(String, OAuthMeta)> {
    let Some(tokens) = account_tokens_in(credentials, spec) else {
        anyhow::bail!(
            "rebon config points at {display} provider `{provider_name}` but you are not \
             signed in to {display} — {hint}",
            display = spec.display_name,
            hint = sign_in_hint(spec),
        );
    };
    if tokens.access_token.is_empty() {
        anyhow::bail!(
            "the stored {} login is empty — {}",
            spec.display_name,
            sign_in_hint(spec)
        );
    }
    Ok(match spec.request_credential {
        RequestCredential::AccessToken => (
            tokens.access_token,
            OAuthMeta {
                provider: spec,
                expires_at_ms: tokens.expires_at,
                refresh_token: tokens.refresh_token,
            },
        ),
        // No session yet is an expired one: the startup check or the first
        // 401 exchanges for it. The OAuth token is what renews it, so it
        // stands where a refresh token would.
        RequestCredential::ExchangedSession { .. } => {
            let expires_at_ms = tokens.session_token.as_ref().and(tokens.session_expires_at);
            (
                tokens.session_token.unwrap_or_default(),
                OAuthMeta {
                    provider: spec,
                    expires_at_ms,
                    refresh_token: Some(tokens.access_token),
                },
            )
        }
    })
}

/// Add the login's required headers to a request's header list, leaving a
/// header the entry already sets alone.
pub fn merge_account_headers(spec: &AccountLoginSpec, headers: &mut Vec<(String, String)>) {
    for (name, value) in spec.request_headers {
        if !headers
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            headers.push((name.to_string(), value.to_string()));
        }
    }
}

// ---------------------------------------------------------------------------
// Renewing a bearer
// ---------------------------------------------------------------------------

/// Mint a fresh bearer for `spec`, persist it, and return it.
///
/// Errors are [`OAuthRefreshError`]s, so a caller can tell "sign in again"
/// from "try again later" with [`super::oauth_refresh_requires_login`].
pub async fn refresh_account(config_dir: &Path, spec: &AccountLoginSpec) -> anyhow::Result<String> {
    let tokens = read_account_tokens(config_dir, spec)?.ok_or_else(|| {
        OAuthRefreshError::unauthorized(format!(
            "not signed in to {} — {}",
            spec.display_name,
            sign_in_hint(spec)
        ))
    })?;
    match (spec.request_credential, spec.storage) {
        (RequestCredential::AccessToken, CredentialSlot::OpenAiOAuth) => {
            let refresh_token = tokens
                .refresh_token
                .filter(|token| !token.is_empty())
                .ok_or_else(|| {
                    OAuthRefreshError::unauthorized(format!(
                        "no {} refresh token is stored — {}",
                        spec.display_name,
                        sign_in_hint(spec)
                    ))
                })?;
            Ok(force_refresh_openai_token(config_dir, &refresh_token)
                .await?
                .tokens
                .access_token)
        }
        (RequestCredential::AccessToken, CredentialSlot::Accounts) => {
            Err(OAuthRefreshError::unauthorized(format!(
                "{} has no refresh path — {}",
                spec.display_name,
                sign_in_hint(spec)
            ))
            .into())
        }
        (RequestCredential::ExchangedSession { exchange_url }, _) => {
            let session = exchange_session(exchange_url, &tokens.access_token).await?;
            let mut tokens = tokens;
            apply_session(&mut tokens, &session);
            write_account_tokens(config_dir, spec, &tokens)?;
            tracing::info!(
                login = spec.id,
                expires_at_ms = session.expires_at_ms,
                "rebon: exchanged a fresh account session token"
            );
            Ok(session.token)
        }
    }
}

/// [`refresh_account`] for a caller with no reactor of its own. Must not be
/// called from inside a tokio runtime.
pub fn refresh_account_blocking(
    config_dir: &Path,
    spec: &AccountLoginSpec,
) -> anyhow::Result<String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(refresh_account(config_dir, spec))
}

/// A session token minted by [`RequestCredential::ExchangedSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangedSession {
    pub token: String,
    pub expires_at_ms: u64,
    /// The account's own API host, when the answer named one over https.
    pub api_base_url: Option<String>,
}

#[derive(Deserialize)]
struct SessionExchangeWire {
    token: String,
    /// Seconds since the Unix epoch.
    expires_at: u64,
    endpoints: Option<SessionEndpointsWire>,
}

#[derive(Deserialize)]
struct SessionEndpointsWire {
    api: Option<String>,
}

/// Read the exchange's answer: `token`, `expires_at` in seconds, and
/// `endpoints.api`. An endpoint that is not https is ignored rather than
/// trusted with the token.
pub fn parse_session_exchange(body: &str) -> Result<ExchangedSession, String> {
    let wire: SessionExchangeWire = serde_json::from_str(body)
        .map_err(|err| format!("unexpected token exchange response: {err}"))?;
    if wire.token.trim().is_empty() {
        return Err("the token exchange answered with an empty token".to_string());
    }
    let api_base_url = wire
        .endpoints
        .and_then(|endpoints| endpoints.api)
        .map(|api| api.trim().trim_end_matches('/').to_string())
        .filter(|api| api.starts_with("https://") && api.len() > "https://".len());
    Ok(ExchangedSession {
        token: wire.token,
        expires_at_ms: wire.expires_at.saturating_mul(1000),
        api_base_url,
    })
}

/// Classify a failed exchange. A revoked token (401), an account without
/// the subscription (403) or a login the endpoint does not know (404) will
/// not recover by waiting; everything else might.
pub fn classify_session_exchange_status(status: u16) -> OAuthRefreshErrorKind {
    match status {
        401 | 403 | 404 => OAuthRefreshErrorKind::Unauthorized,
        _ => OAuthRefreshErrorKind::Transient,
    }
}

/// `GET exchange_url` with the OAuth token and read the session it mints.
pub async fn exchange_session(
    exchange_url: &str,
    oauth_token: &str,
) -> Result<ExchangedSession, OAuthRefreshError> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(OAUTH_REFRESH_TIMEOUT_MS))
        .user_agent(REBON_EDITOR_VERSION)
        .build()
        .map_err(|err| OAuthRefreshError::transient(format!("build HTTP client: {err}")))?;
    let response = http
        .get(exchange_url)
        .header("Authorization", format!("token {oauth_token}"))
        .header("Accept", "application/json")
        .header("Editor-Version", REBON_EDITOR_VERSION)
        .header("Editor-Plugin-Version", REBON_PLUGIN_VERSION)
        .header("X-GitHub-Api-Version", "2025-04-01")
        .send()
        .await
        .map_err(|err| OAuthRefreshError::transient(format!("token exchange failed: {err}")))?;
    let status = response.status();
    let body = response.text().await.map_err(|err| {
        OAuthRefreshError::transient(format!("read token exchange response: {err}"))
    })?;
    if !status.is_success() {
        let kind = classify_session_exchange_status(status.as_u16());
        let reason = match status.as_u16() {
            401 => "the account token was revoked or has expired",
            403 | 404 => "this account has no access (is the subscription active?)",
            _ => "the service did not answer",
        };
        return Err(OAuthRefreshError::new(
            kind,
            format!("token exchange returned {status}: {reason}"),
        ));
    }
    parse_session_exchange(&body).map_err(OAuthRefreshError::transient)
}

fn apply_session(tokens: &mut AccountTokens, session: &ExchangedSession) {
    tokens.session_token = Some(session.token.clone());
    tokens.session_expires_at = Some(session.expires_at_ms);
    if session.api_base_url.is_some() {
        tokens.api_base_url = session.api_base_url.clone();
    }
}

// ---------------------------------------------------------------------------
// Finishing a login
// ---------------------------------------------------------------------------

/// Everything after the user approved: mint the request credential when the
/// login needs one, store the tokens, upsert the provider entry (and make it
/// the active one), and fill its model list from the API when it has one.
///
/// The ChatGPT login finishes through the onboarding plugin's own exchange,
/// which this does not replace; this is for every other login.
pub async fn complete_account_login(
    config_dir: &Path,
    spec: &AccountLoginSpec,
    mut tokens: AccountTokens,
) -> anyhow::Result<AccountTokens> {
    if let RequestCredential::ExchangedSession { exchange_url } = spec.request_credential {
        // Exchanging before anything is written is also the subscription
        // check: an account without one is refused here, not on the first
        // model request.
        let session = exchange_session(exchange_url, &tokens.access_token).await?;
        apply_session(&mut tokens, &session);
    }
    write_account_tokens(config_dir, spec, &tokens)?;
    upsert_account_provider_in(config_dir, spec, tokens.api_base_url.as_deref())?;
    if let Err(err) = sync_account_models(config_dir, spec, &tokens).await {
        tracing::warn!(
            login = spec.id,
            error = %err,
            "rebon: could not list the account's models; keeping the seeded list"
        );
    }
    tracing::info!(login = spec.id, "rebon: account login complete");
    Ok(tokens)
}

/// Upsert the login's provider entry and make it the active provider.
///
/// Idempotent. An entry with the same name is rewritten to the login's
/// format, endpoint and sentinel, keeping models the user added; unrelated
/// entries are untouched. `base_url` is the endpoint the login reported for
/// this account, when it reported one.
pub fn upsert_account_provider_in(
    config_dir: &Path,
    spec: &AccountLoginSpec,
    base_url: Option<&str>,
) -> anyhow::Result<()> {
    if spec.is_codex() {
        let models: Vec<String> = spec.provider.models.iter().map(|m| m.to_string()).collect();
        return upsert_openai_oauth_provider_in(config_dir, &models);
    }
    let entry = &spec.provider;
    let base_url = base_url.unwrap_or(entry.base_url).to_string();
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = entry.name.to_lowercase();
    match config
        .custom_providers
        .iter_mut()
        .find(|p| p.name.to_lowercase() == name_lower)
    {
        Some(existing) => {
            existing.format = Some(entry.format.to_string());
            existing.vendor = entry.vendor.map(str::to_string);
            existing.base_url = base_url;
            existing.api_key = spec.sentinel.to_string();
            for model in entry.models {
                existing.models.ensure_id(model);
            }
            if existing.model.is_empty() || !existing.models.contains_id(&existing.model) {
                existing.model = entry.default_model.to_string();
            }
        }
        None => {
            let mut models = CustomProviderModels::default();
            for model in entry.models {
                models.ensure_id(model);
            }
            config.custom_providers.push(CustomProvider {
                name: entry.name.to_string(),
                format: Some(entry.format.to_string()),
                vendor: entry.vendor.map(str::to_string),
                base_url,
                api_key: spec.sentinel.to_string(),
                model: entry.default_model.to_string(),
                models,
                model_profiles: ModelProfileMap::default(),
                options: ProviderOptions::default(),
                request_scoped_transient_context: None,
                use_websocket: false,
                thinking_enabled: None,
                thinking_effort: None,
                reasoning_mode: None,
                extra: serde_json::Map::new(),
            });
        }
    }
    config.active_custom_provider = Some(entry.name.to_string());
    write_config_roundtrip(config_dir, &config)
}

/// List the account's models from its API and fold them into the entry.
async fn sync_account_models(
    config_dir: &Path,
    spec: &AccountLoginSpec,
    tokens: &AccountTokens,
) -> anyhow::Result<()> {
    let bearer = match spec.request_credential {
        RequestCredential::AccessToken => tokens.access_token.clone(),
        RequestCredential::ExchangedSession { .. } => {
            tokens.session_token.clone().unwrap_or_default()
        }
    };
    let base_url = tokens
        .api_base_url
        .clone()
        .unwrap_or_else(|| spec.provider.base_url.to_string());
    let mut extra_headers = Vec::new();
    merge_account_headers(spec, &mut extra_headers);
    let request = rebon_api::ModelDiscoveryRequest {
        vendor: rebon_api::ProviderVendor::resolve(spec.provider.vendor, &base_url),
        wire: wire_family_for_format(spec.provider.format),
        base_url,
        api_key: bearer,
        extra_headers,
    };
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(OAUTH_REFRESH_TIMEOUT_MS))
        .build()?;
    let discovery = rebon_api::discover_models(&http, &request).await?;
    if discovery.models.is_empty() {
        return Ok(());
    }
    let mut config = read_config_roundtrip(config_dir)?;
    let name_lower = spec.provider.name.to_lowercase();
    let Some(provider) = config
        .custom_providers
        .iter_mut()
        .find(|p| p.name.to_lowercase() == name_lower)
    else {
        return Ok(());
    };
    merge_discovered_models(
        provider,
        &discovery.models,
        Some(spec.provider.default_model),
    );
    write_config_roundtrip(config_dir, &config)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// One login as `rebon login --status` and the settings pages show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLoginStatus {
    pub spec: &'static AccountLoginSpec,
    pub signed_in: bool,
    /// When the stored token stops working without a refresh. `None` for a
    /// login whose account token does not expire (its session renews on
    /// its own), or when no expiry was recorded.
    pub expires_at_ms: Option<u64>,
    /// Whether a provider entry names this login.
    pub provider_configured: bool,
    /// Whether that entry is the active provider.
    pub active: bool,
}

/// Every login's status, in picker order.
pub fn account_login_statuses_in(config_dir: &Path) -> anyhow::Result<Vec<AccountLoginStatus>> {
    let credentials = read_credentials(config_dir)?;
    let config = read_config_roundtrip(config_dir).ok();
    let providers: BTreeMap<String, bool> = config
        .as_ref()
        .map(|config| {
            config
                .custom_providers
                .iter()
                .filter_map(|provider| {
                    let spec = account_login_for_api_key(&provider.api_key)?;
                    let active =
                        config.active_custom_provider.as_deref() == Some(provider.name.as_str());
                    Some((spec.id.to_string(), active))
                })
                .fold(BTreeMap::new(), |mut acc, (id, active)| {
                    let slot = acc.entry(id).or_insert(false);
                    *slot |= active;
                    acc
                })
        })
        .unwrap_or_default();
    Ok(ACCOUNT_LOGINS
        .iter()
        .map(|spec| {
            let tokens = account_tokens_in(&credentials, spec)
                .filter(|tokens| !tokens.access_token.is_empty());
            let expires_at_ms = match spec.request_credential {
                RequestCredential::AccessToken => tokens.as_ref().and_then(|t| t.expires_at),
                RequestCredential::ExchangedSession { .. } => None,
            };
            AccountLoginStatus {
                spec,
                signed_in: tokens.is_some(),
                expires_at_ms,
                provider_configured: providers.contains_key(spec.id),
                active: providers.get(spec.id).copied().unwrap_or(false),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests;
