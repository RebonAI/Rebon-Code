//! Coverage matrix for account logins.
//!
//! * The table: unique ids, aliases and sentinels; every sentinel and every
//!   `$OAUTH:<id>` resolves back to its row; the ChatGPT row is the constants
//!   the refresh path has always used; the Copilot entry is its vendor.
//! * Storage: a second login lands beside `openaiOAuth` without disturbing
//!   it or unknown keys (top level and per entry); a file with only the
//!   legacy key still reads; sign-out removes exactly one login.
//! * Resolution per login: the ChatGPT token, the Copilot session token and
//!   headers, no session yet, not signed in, an unknown sentinel, and an
//!   entry header beating a login header.
//! * The Codex-only gates stay Codex-only, even for another login pointed at
//!   the Codex host.
//! * The session exchange: wire parsing, status classes, a real request
//!   against a loopback server (success, revoked, outage), a refresh that
//!   persists, the startup check routing to it, and a whole login finish
//!   (exchange, store, upsert, model listing).
//! * The client id override: the table's by default, a configured one for
//!   a device login, never for the ChatGPT login or an implausible value.
//! * Upsert and status.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::*;
use crate::{
    check_and_refresh_if_needed, credentials_json_path, is_first_party_openai_route,
    list_custom_providers_from, oauth_refresh_requires_login, provider_model_choices,
    provider_setup_status_in, resolve_from_dir, resolve_from_dir_with, CustomProviderInfo,
    ProviderFormat, ProviderSetupCredential,
};

fn copilot() -> &'static AccountLoginSpec {
    account_login(COPILOT_LOGIN_ID).expect("copilot row")
}

fn write_json(path: &Path, value: serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn config_with(dir: &Path, name: &str, api_key: &str, base_url: &str, format: &str) {
    write_json(
        &dir.join("config.json"),
        serde_json::json!({
            "activeCustomProvider": name,
            "customProviders": [{
                "name": name,
                "format": format,
                "baseUrl": base_url,
                "apiKey": api_key,
                "model": "m"
            }]
        }),
    );
}

fn signed_in_copilot(session: Option<(&str, u64)>) -> AccountTokens {
    let mut tokens = AccountTokens::new("gho_account".into(), None, None);
    if let Some((token, expires_at)) = session {
        tokens.session_token = Some(token.into());
        tokens.session_expires_at = Some(expires_at);
    }
    tokens
}

// ── The table ──────────────────────────────────────────────────

#[test]
fn ids_aliases_and_sentinels_are_unique_and_resolve_back() {
    let mut names = Vec::new();
    let mut sentinels = Vec::new();
    for spec in ACCOUNT_LOGINS {
        names.push(spec.id.to_ascii_lowercase());
        names.extend(spec.aliases.iter().map(|a| a.to_ascii_lowercase()));
        sentinels.push(spec.sentinel);

        assert_eq!(account_login(spec.id).map(|s| s.id), Some(spec.id));
        assert_eq!(
            account_login(&spec.id.to_ascii_uppercase()).map(|s| s.id),
            Some(spec.id)
        );
        for alias in spec.aliases {
            assert_eq!(account_login(alias).map(|s| s.id), Some(spec.id));
        }
        assert_eq!(
            account_login_for_api_key(spec.sentinel).map(|s| s.id),
            Some(spec.id)
        );
        let generic = format!("{ACCOUNT_SENTINEL_PREFIX}{}", spec.id);
        assert_eq!(
            account_login_for_api_key(&generic).map(|s| s.id),
            Some(spec.id)
        );
    }
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), total, "an id or alias names two logins");
    let total = sentinels.len();
    sentinels.sort();
    sentinels.dedup();
    assert_eq!(sentinels.len(), total);
}

#[test]
fn every_row_is_complete_and_https() {
    for spec in ACCOUNT_LOGINS {
        assert!(!spec.client_id.is_empty(), "{}", spec.id);
        assert!(!spec.scopes.is_empty(), "{}", spec.id);
        assert!(spec.token_url.starts_with("https://"), "{}", spec.id);
        assert!(!spec.provenance.is_empty(), "{}", spec.id);
        assert!(
            spec.provider.models.contains(&spec.provider.default_model),
            "{}: the default model is not seeded",
            spec.id
        );
        assert!(
            crate::VALID_PROVIDER_FORMATS.contains(&spec.provider.format),
            "{}",
            spec.id
        );
        match spec.flow {
            AccountFlow::PkceLoopback { authorize_url, .. } => {
                assert!(authorize_url.starts_with("https://"))
            }
            AccountFlow::DeviceCode {
                device_authorization_url,
            } => assert!(device_authorization_url.starts_with("https://")),
        }
        if let RequestCredential::ExchangedSession { exchange_url } = spec.request_credential {
            assert!(exchange_url.starts_with("https://"), "{}", spec.id);
        }
        // Only the ChatGPT login keeps the legacy slot and the legacy
        // sentinel; every other one is generic.
        if spec.is_codex() {
            assert_eq!(spec.storage, CredentialSlot::OpenAiOAuth);
        } else {
            assert_eq!(spec.storage, CredentialSlot::Accounts);
            assert_eq!(
                spec.sentinel,
                format!("{ACCOUNT_SENTINEL_PREFIX}{}", spec.id)
            );
        }
    }
}

/// The ChatGPT row is the constants the refresh path and the synthetic
/// entry have always used — one fact, not a second copy that can drift.
#[test]
fn the_chatgpt_row_is_the_existing_codex_constants() {
    let codex = codex_login();
    assert_eq!(codex.id, "openai");
    assert_eq!(codex.token_url, crate::OPENAI_OAUTH_TOKEN_URL);
    assert_eq!(codex.client_id, crate::OPENAI_OAUTH_CLIENT_ID);
    assert_eq!(codex.sentinel, "$OPENAI_OAUTH_TOKEN");
    assert_eq!(codex.provider.name, crate::OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(
        codex.provider.base_url,
        crate::OPENAI_OAUTH_PROVIDER_BASE_URL
    );
    assert_eq!(codex.provider.models, crate::OPENAI_OAUTH_PROVIDER_MODELS);
    assert!(codex.request_headers.is_empty());
    assert!(codex.answers_to("Codex") && codex.answers_to("chatgpt"));
}

#[test]
fn the_copilot_entry_is_its_vendor_and_names_rebon_not_another_editor() {
    let spec = copilot();
    assert_eq!(
        rebon_api::ProviderVendor::resolve(spec.provider.vendor, spec.provider.base_url),
        rebon_api::ProviderVendor::GithubCopilot
    );
    assert_eq!(
        rebon_api::ProviderVendor::detect(spec.provider.base_url),
        rebon_api::ProviderVendor::GithubCopilot
    );
    let editor = spec
        .request_headers
        .iter()
        .find(|(name, _)| *name == "Editor-Version")
        .expect("Copilot needs Editor-Version");
    assert!(editor.1.starts_with("Rebon/"), "{}", editor.1);
    assert!(!spec
        .request_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("Copilot-Integration-Id")));
}

#[test]
fn sentinels_match_exactly_and_unknown_logins_are_still_sentinels() {
    assert!(account_login_for_api_key("$OPENAI_OAUTH_TOKEN ").is_none());
    assert!(account_login_for_api_key("sk-live").is_none());
    assert!(account_login_for_api_key("$OAUTH:nope").is_none());
    assert!(account_login_for_api_key("$OAUTH:Copilot").is_none());
    assert!(is_account_sentinel("$OAUTH:nope"));
    assert!(is_account_sentinel("$OPENAI_OAUTH_TOKEN"));
    assert!(!is_account_sentinel("sk-live"));
}

// ── Client id override ─────────────────────────────────────────

#[test]
fn the_table_client_id_is_used_until_config_names_another() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        account_client_id(dir.path(), copilot()),
        copilot().client_id
    );

    write_json(
        &dir.path().join("config.json"),
        serde_json::json!({
            "accountLogins": { "copilot": { "clientId": "  Iv1.0123456789abcdef " } }
        }),
    );
    assert_eq!(
        account_client_id(dir.path(), copilot()),
        "Iv1.0123456789abcdef"
    );
}

/// The ChatGPT login's id is bound to its loopback redirect registration,
/// and a value that cannot be a client id is not sent.
#[test]
fn the_override_skips_the_chatgpt_login_and_implausible_values() {
    let dir = tempfile::tempdir().unwrap();
    for value in [
        serde_json::json!(""),
        serde_json::json!("has space"),
        serde_json::json!("x".repeat(129)),
        serde_json::json!(42),
    ] {
        write_json(
            &dir.path().join("config.json"),
            serde_json::json!({
                "accountLogins": {
                    "openai": { "clientId": "app_other" },
                    "copilot": { "clientId": value }
                }
            }),
        );
        assert_eq!(
            account_client_id(dir.path(), copilot()),
            copilot().client_id
        );
        assert_eq!(
            account_client_id(dir.path(), codex_login()),
            crate::OPENAI_OAUTH_CLIENT_ID
        );
    }
}

/// `accountLogins` is a key this crate does not model; a provider write
/// must carry it through like any other unknown key.
#[test]
fn the_override_survives_a_provider_upsert() {
    let dir = tempfile::tempdir().unwrap();
    write_json(
        &dir.path().join("config.json"),
        serde_json::json!({
            "accountLogins": { "copilot": { "clientId": "Iv1.custom" } }
        }),
    );
    upsert_account_provider_in(dir.path(), copilot(), None).unwrap();
    assert_eq!(account_client_id(dir.path(), copilot()), "Iv1.custom");
}

// ── Storage ────────────────────────────────────────────────────

#[test]
fn a_second_login_lands_beside_the_legacy_key_and_unknown_keys_survive() {
    let dir = tempfile::tempdir().unwrap();
    let path = credentials_json_path(dir.path());
    write_json(
        &path,
        serde_json::json!({
            "openaiOAuth": {"accessToken": "a", "refreshToken": "r", "expiresAt": 1},
            "claudeAiOauth": {"keep": true},
            "oauthAccounts": {"future": {"accessToken": "f", "vendorOnly": 7}}
        }),
    );

    write_account_tokens(dir.path(), copilot(), &signed_in_copilot(Some(("tid", 5)))).unwrap();

    let json = read_json(&path);
    assert_eq!(
        json["openaiOAuth"],
        serde_json::json!({"accessToken": "a", "refreshToken": "r", "expiresAt": 1})
    );
    assert_eq!(json["claudeAiOauth"], serde_json::json!({"keep": true}));
    assert_eq!(
        json["oauthAccounts"]["future"],
        serde_json::json!({"accessToken": "f", "vendorOnly": 7})
    );
    assert_eq!(
        json["oauthAccounts"]["copilot"],
        serde_json::json!({
            "accessToken": "gho_account",
            "sessionToken": "tid",
            "sessionExpiresAt": 5
        })
    );
    // And it reads back the way it was written.
    assert_eq!(
        read_account_tokens(dir.path(), copilot()).unwrap(),
        Some(signed_in_copilot(Some(("tid", 5))))
    );
}

#[test]
fn unknown_keys_inside_an_entry_survive_a_refresh_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = credentials_json_path(dir.path());
    write_json(
        &path,
        serde_json::json!({"oauthAccounts": {"copilot": {"accessToken": "g", "later": [1]}}}),
    );
    let mut tokens = read_account_tokens(dir.path(), copilot()).unwrap().unwrap();
    tokens.session_token = Some("s".into());
    write_account_tokens(dir.path(), copilot(), &tokens).unwrap();
    assert_eq!(
        read_json(&path)["oauthAccounts"]["copilot"]["later"],
        serde_json::json!([1])
    );
}

#[test]
fn a_file_with_only_the_legacy_key_still_reads_as_the_chatgpt_login() {
    let dir = tempfile::tempdir().unwrap();
    write_json(
        &credentials_json_path(dir.path()),
        serde_json::json!({"openaiOAuth": {"accessToken": "a", "expiresAt": 9}}),
    );
    let tokens = read_account_tokens(dir.path(), codex_login())
        .unwrap()
        .unwrap();
    assert_eq!(tokens, AccountTokens::new("a".into(), None, Some(9)));
    assert_eq!(read_account_tokens(dir.path(), copilot()).unwrap(), None);
}

/// The ChatGPT login is written where older binaries look for it.
#[test]
fn the_chatgpt_login_writes_the_legacy_key_not_the_account_map() {
    let dir = tempfile::tempdir().unwrap();
    write_account_tokens(
        dir.path(),
        codex_login(),
        &AccountTokens::new("a".into(), Some("r".into()), Some(3)),
    )
    .unwrap();
    let json = read_json(&credentials_json_path(dir.path()));
    assert_eq!(
        json,
        serde_json::json!({"openaiOAuth": {"accessToken": "a", "refreshToken": "r", "expiresAt": 3}})
    );
}

#[test]
fn signing_out_removes_exactly_one_login() {
    let dir = tempfile::tempdir().unwrap();
    let path = credentials_json_path(dir.path());
    write_json(
        &path,
        serde_json::json!({
            "openaiOAuth": {"accessToken": "a"},
            "oauthAccounts": {"copilot": {"accessToken": "g"}, "other": {"accessToken": "o"}},
            "claudeAiOauth": 1
        }),
    );

    assert!(sign_out_account(dir.path(), copilot()).unwrap());
    let json = read_json(&path);
    assert_eq!(json["openaiOAuth"]["accessToken"], "a");
    assert_eq!(json["oauthAccounts"]["other"]["accessToken"], "o");
    assert!(json["oauthAccounts"].get("copilot").is_none());
    assert_eq!(json["claudeAiOauth"], 1);

    assert!(sign_out_account(dir.path(), codex_login()).unwrap());
    let json = read_json(&path);
    assert!(json.get("openaiOAuth").is_none());
    assert_eq!(json["oauthAccounts"]["other"]["accessToken"], "o");

    // Signing out twice is not an error, and writes nothing.
    let before = std::fs::read(&path).unwrap();
    assert!(!sign_out_account(dir.path(), copilot()).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn signing_out_with_no_credentials_file_creates_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!sign_out_account(dir.path(), copilot()).unwrap());
    assert!(!credentials_json_path(dir.path()).exists());
}

// ── Resolution ─────────────────────────────────────────────────

#[test]
fn the_chatgpt_sentinel_resolves_to_the_codex_login() {
    let dir = tempfile::tempdir().unwrap();
    config_with(
        dir.path(),
        "openai",
        "$OPENAI_OAUTH_TOKEN",
        crate::OPENAI_OAUTH_PROVIDER_BASE_URL,
        "openai-responses",
    );
    write_json(
        &credentials_json_path(dir.path()),
        serde_json::json!({"openaiOAuth": {"accessToken": "chat", "refreshToken": "r", "expiresAt": 7}}),
    );
    let resolved = resolve_from_dir(dir.path()).unwrap().unwrap();
    assert_eq!(resolved.api_key, "chat");
    let oauth = resolved.oauth.as_ref().unwrap();
    assert!(oauth.is_codex());
    assert_eq!(oauth.expires_at_ms, Some(7));
    assert!(resolved.has_codex_login());
    assert!(resolved.extra_headers.is_empty());
}

#[test]
fn the_copilot_sentinel_resolves_to_the_session_token_with_its_headers() {
    let dir = tempfile::tempdir().unwrap();
    config_with(
        dir.path(),
        "copilot",
        "$OAUTH:copilot",
        "https://api.githubcopilot.com",
        "openai",
    );
    write_account_tokens(
        dir.path(),
        copilot(),
        &signed_in_copilot(Some(("tid=1;exp=2", 99))),
    )
    .unwrap();

    let resolved = resolve_from_dir(dir.path()).unwrap().unwrap();
    assert_eq!(resolved.api_key, "tid=1;exp=2");
    let oauth = resolved.oauth.as_ref().unwrap();
    assert!(!oauth.is_codex());
    assert_eq!(oauth.provider.id, "copilot");
    assert_eq!(oauth.expires_at_ms, Some(99));
    assert_eq!(oauth.refresh_token.as_deref(), Some("gho_account"));
    assert!(!resolved.has_codex_login());
    assert_eq!(
        resolved.vendor,
        rebon_api::ProviderVendor::GithubCopilot,
        "detected from the host"
    );
    for (name, value) in copilot().request_headers {
        assert!(
            resolved
                .extra_headers
                .iter()
                .any(|(n, v)| n == name && v == value),
            "{name}"
        );
    }
}

#[test]
fn a_copilot_login_without_a_session_yet_resolves_as_expired() {
    let dir = tempfile::tempdir().unwrap();
    config_with(
        dir.path(),
        "copilot",
        "$OAUTH:copilot",
        "https://api.githubcopilot.com",
        "openai",
    );
    write_account_tokens(dir.path(), copilot(), &signed_in_copilot(None)).unwrap();
    let resolved = resolve_from_dir(dir.path()).unwrap().unwrap();
    assert_eq!(resolved.api_key, "");
    let oauth = resolved.oauth.unwrap();
    assert_eq!(oauth.expires_at_ms, None);
    assert!(crate::is_token_expired(oauth.expires_at_ms));
}

#[test]
fn a_login_that_is_not_signed_in_fails_with_the_sign_in_command() {
    let dir = tempfile::tempdir().unwrap();
    config_with(
        dir.path(),
        "copilot",
        "$OAUTH:copilot",
        "https://api.githubcopilot.com",
        "openai",
    );
    let err = resolve_from_dir(dir.path()).unwrap_err().to_string();
    assert!(err.contains("rebon login copilot"), "{err}");

    // An empty stored account token is the same answer.
    write_account_tokens(
        dir.path(),
        copilot(),
        &AccountTokens::new(String::new(), None, None),
    )
    .unwrap();
    let err = resolve_from_dir(dir.path()).unwrap_err().to_string();
    assert!(err.contains("rebon login copilot"), "{err}");
}

#[test]
fn an_unknown_login_sentinel_is_refused_not_sent_as_a_key() {
    let dir = tempfile::tempdir().unwrap();
    config_with(
        dir.path(),
        "x",
        "$OAUTH:gemini",
        "https://example.com/v1",
        "openai",
    );
    let err = resolve_from_dir(dir.path()).unwrap_err().to_string();
    assert!(err.contains("unknown account login"), "{err}");
    assert!(err.contains("copilot"), "{err}");
}

#[test]
fn a_header_the_entry_sets_beats_the_logins_own() {
    let dir = tempfile::tempdir().unwrap();
    write_json(
        &dir.path().join("config.json"),
        serde_json::json!({
            "activeCustomProvider": "copilot",
            "customProviders": [{
                "name": "copilot",
                "format": "openai",
                "baseUrl": "https://api.githubcopilot.com",
                "apiKey": "$OAUTH:copilot",
                "model": "m",
                "options": {"headers": {"editor-version": "Custom/1"}}
            }]
        }),
    );
    write_account_tokens(dir.path(), copilot(), &signed_in_copilot(Some(("t", 1)))).unwrap();
    let resolved = resolve_from_dir(dir.path()).unwrap().unwrap();
    let editor: Vec<&(String, String)> = resolved
        .extra_headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("editor-version"))
        .collect();
    assert_eq!(editor.len(), 1);
    assert_eq!(editor[0].1, "Custom/1");
}

// ── Codex-only gates ───────────────────────────────────────────

/// Regression: generalising OAuth must not hand Codex-only features to
/// every login. A Copilot login even pointed at the Codex host is not
/// OpenAI's own route.
#[test]
fn codex_only_gates_stay_codex_only() {
    let dir = tempfile::tempdir().unwrap();
    write_account_tokens(dir.path(), copilot(), &signed_in_copilot(Some(("t", 1)))).unwrap();
    write_json(
        &dir.path().join("config.json"),
        serde_json::json!({
            "customProviders": [
                {"name": "copilot", "format": "openai-responses",
                 "baseUrl": crate::OPENAI_OAUTH_PROVIDER_BASE_URL,
                 "apiKey": "$OAUTH:copilot", "model": "m"},
                {"name": "openai", "format": "openai-responses",
                 "baseUrl": crate::OPENAI_OAUTH_PROVIDER_BASE_URL,
                 "apiKey": "$OPENAI_OAUTH_TOKEN", "model": "m"}
            ]
        }),
    );
    let mut credentials = read_json(&credentials_json_path(dir.path()));
    credentials["openaiOAuth"] = serde_json::json!({"accessToken": "chat"});
    write_json(&credentials_json_path(dir.path()), credentials);

    let copilot_on_codex_host = resolve_from_dir_with(dir.path(), Some("copilot"))
        .unwrap()
        .unwrap();
    assert!(copilot_on_codex_host.oauth.is_some());
    assert!(!copilot_on_codex_host.has_codex_login());
    assert!(!is_first_party_openai_route(
        copilot_on_codex_host.format,
        &copilot_on_codex_host.base_url,
        copilot_on_codex_host.has_codex_login(),
        false,
    ));

    let codex = resolve_from_dir_with(dir.path(), Some("openai"))
        .unwrap()
        .unwrap();
    assert!(codex.has_codex_login());
    assert!(is_first_party_openai_route(
        ProviderFormat::OpenaiResponses,
        &codex.base_url,
        codex.has_codex_login(),
        false,
    ));
}

// ── The session exchange ───────────────────────────────────────

#[test]
fn a_session_answer_is_read_in_seconds_with_its_endpoint() {
    let session = parse_session_exchange(
        r#"{"token":"tid=1","expires_at":1700000000,"refresh_in":1500,
            "endpoints":{"api":"https://api.individual.githubcopilot.com/"}}"#,
    )
    .unwrap();
    assert_eq!(session.token, "tid=1");
    assert_eq!(session.expires_at_ms, 1_700_000_000_000);
    assert_eq!(
        session.api_base_url.as_deref(),
        Some("https://api.individual.githubcopilot.com")
    );
}

#[test]
fn a_session_answer_without_an_https_endpoint_keeps_no_endpoint() {
    for endpoints in [
        r#""endpoints":{"api":"http://plain.example"}"#,
        r#""endpoints":{}"#,
        r#""endpoints":{"api":"https://"}"#,
        r#""other":1"#,
    ] {
        let body = format!(r#"{{"token":"t","expires_at":1,{endpoints}}}"#);
        assert_eq!(
            parse_session_exchange(&body).unwrap().api_base_url,
            None,
            "{body}"
        );
    }
}

#[test]
fn a_session_answer_without_a_token_or_expiry_is_refused() {
    for body in [
        r#"{"expires_at":1}"#,
        r#"{"token":"  ","expires_at":1}"#,
        r#"{"token":"t"}"#,
        r#"{"token":"t","expires_at":"soon"}"#,
        "not json",
    ] {
        assert!(parse_session_exchange(body).is_err(), "{body}");
    }
}

#[test]
fn exchange_statuses_split_into_sign_in_again_and_try_later() {
    for status in [401, 403, 404] {
        assert_eq!(
            classify_session_exchange_status(status),
            OAuthRefreshErrorKind::Unauthorized,
            "{status}"
        );
    }
    for status in [408, 429, 500, 502, 503] {
        assert_eq!(
            classify_session_exchange_status(status),
            OAuthRefreshErrorKind::Transient,
            "{status}"
        );
    }
}

/// One HTTP exchange the loopback server saw.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A loopback HTTP server answering by path, one connection at a time,
/// for as many connections as `answers` allows.
fn serve(
    answers: Vec<(&'static str, u16, String)>,
) -> (String, Arc<Mutex<Vec<Seen>>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let handle = std::thread::spawn(move || {
        let mut answers = answers;
        while !answers.is_empty() {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let text = String::from_utf8_lossy(&buffer).to_string();
            let mut lines = text.lines();
            let path = lines
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let headers = lines
                .take_while(|line| !line.is_empty())
                .filter_map(|line| {
                    let (n, v) = line.split_once(':')?;
                    Some((n.trim().to_string(), v.trim().to_string()))
                })
                .collect();
            recorded.lock().unwrap().push(Seen {
                path: path.clone(),
                headers,
            });
            let index = answers
                .iter()
                .position(|(prefix, _, _)| path.starts_with(prefix))
                .unwrap_or_else(|| panic!("no answer for {path}"));
            let (_, status, body) = answers.remove(index);
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (base, seen, handle)
}

/// A Copilot row whose endpoints point at the loopback server.
fn copilot_at(base: &str) -> &'static AccountLoginSpec {
    let exchange_url: &'static str = Box::leak(format!("{base}/copilot_internal/v2/token").into());
    let api: &'static str = Box::leak(base.to_string().into_boxed_str());
    Box::leak(Box::new(AccountLoginSpec {
        request_credential: RequestCredential::ExchangedSession { exchange_url },
        provider: AccountProviderEntry {
            base_url: api,
            ..copilot().provider
        },
        ..*copilot()
    }))
}

fn session_body(token: &str) -> String {
    format!(r#"{{"token":"{token}","expires_at":4102444800,"refresh_in":1500}}"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_exchange_sends_the_account_token_and_names_rebon() {
    let (base, seen, server) = serve(vec![("/copilot_internal", 200, session_body("tid"))]);
    let spec = copilot_at(&base);
    let RequestCredential::ExchangedSession { exchange_url } = spec.request_credential else {
        unreachable!("the Copilot row exchanges")
    };

    let session = exchange_session(exchange_url, "gho_x").await.unwrap();
    server.join().unwrap();

    assert_eq!(session.token, "tid");
    assert_eq!(session.expires_at_ms, 4_102_444_800_000);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].header("authorization"), Some("token gho_x"));
    assert!(seen[0]
        .header("editor-version")
        .is_some_and(|v| v.starts_with("Rebon/")));
    assert!(seen[0]
        .header("user-agent")
        .is_some_and(|v| v.starts_with("Rebon/")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_account_token_asks_for_a_new_login_and_an_outage_does_not() {
    let (base, _seen, server) = serve(vec![
        ("/copilot_internal", 401, "{}".into()),
        ("/copilot_internal", 503, "{}".into()),
    ]);
    let spec = copilot_at(&base);
    let RequestCredential::ExchangedSession { exchange_url } = spec.request_credential else {
        unreachable!()
    };

    let revoked = exchange_session(exchange_url, "g").await.unwrap_err();
    assert!(revoked.requires_login());
    let outage = exchange_session(exchange_url, "g").await.unwrap_err();
    assert!(!outage.requires_login());
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshing_a_session_persists_it_and_keeps_the_account_token() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _seen, server) = serve(vec![("/copilot_internal", 200, session_body("new"))]);
    let spec = copilot_at(&base);
    write_account_tokens(dir.path(), spec, &signed_in_copilot(Some(("old", 1)))).unwrap();

    let bearer = refresh_account(dir.path(), spec).await.unwrap();
    server.join().unwrap();

    assert_eq!(bearer, "new");
    let stored = read_account_tokens(dir.path(), spec).unwrap().unwrap();
    assert_eq!(stored.access_token, "gho_account");
    assert_eq!(stored.session_token.as_deref(), Some("new"));
    assert_eq!(stored.session_expires_at, Some(4_102_444_800_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshing_without_a_login_asks_for_one() {
    let dir = tempfile::tempdir().unwrap();
    let err = refresh_account(dir.path(), copilot()).await.unwrap_err();
    assert!(oauth_refresh_requires_login(&err), "{err}");
}

/// The startup check routes an expired Copilot session to the exchange, not
/// to OpenAI's refresh-token grant.
#[tokio::test(flavor = "multi_thread")]
async fn the_startup_check_renews_an_expired_session_through_the_exchange() {
    let dir = tempfile::tempdir().unwrap();
    let (base, seen, server) = serve(vec![("/copilot_internal", 200, session_body("fresh"))]);
    let spec = copilot_at(&base);
    write_account_tokens(dir.path(), spec, &signed_in_copilot(Some(("old", 0)))).unwrap();

    let expired = OAuthMeta {
        provider: spec,
        expires_at_ms: Some(0),
        refresh_token: Some("gho_account".into()),
    };
    let fresh = check_and_refresh_if_needed(dir.path(), &expired)
        .await
        .unwrap();
    server.join().unwrap();
    assert_eq!(fresh.as_deref(), Some("fresh"));
    assert_eq!(seen.lock().unwrap().len(), 1);

    // A live session is left alone: no request at all.
    let live = OAuthMeta {
        expires_at_ms: Some(u64::MAX),
        ..expired
    };
    assert_eq!(
        check_and_refresh_if_needed(dir.path(), &live)
            .await
            .unwrap(),
        None
    );
}

/// The whole finish: exchange, store, upsert, list.
#[tokio::test(flavor = "multi_thread")]
async fn finishing_a_login_stores_upserts_activates_and_lists_models() {
    let dir = tempfile::tempdir().unwrap();
    write_json(
        &dir.path().join("config.json"),
        serde_json::json!({
            "activeCustomProvider": "mine",
            "customProviders": [{"name": "mine", "baseUrl": "https://example.com/v1",
                                 "apiKey": "sk", "model": "x"}]
        }),
    );
    let models = r#"{"object":"list","data":[{"id":"gpt-5-mini"},{"id":"claude-opus-5"},{"id":"text-embedding-3-small"}]}"#;
    let (base, seen, server) = serve(vec![
        ("/copilot_internal", 200, session_body("sess")),
        ("/models", 200, models.to_string()),
    ]);
    let spec = copilot_at(&base);

    complete_account_login(
        dir.path(),
        spec,
        AccountTokens::new("gho_new".into(), None, None),
    )
    .await
    .unwrap();
    server.join().unwrap();

    let stored = read_account_tokens(dir.path(), spec).unwrap().unwrap();
    assert_eq!(stored.access_token, "gho_new");
    assert_eq!(stored.session_token.as_deref(), Some("sess"));

    let providers = list_custom_providers_from(dir.path());
    let entry = providers.iter().find(|p| p.name == "copilot").unwrap();
    assert_eq!(entry.api_key, "$OAUTH:copilot");
    assert_eq!(entry.format, "openai");
    assert_eq!(entry.base_url, base);
    assert!(entry.models.contains(&"claude-opus-5".to_string()));
    assert!(!entry.models.iter().any(|m| m.contains("embedding")));
    assert!(providers.iter().any(|p| p.name == "mine"));
    assert_eq!(
        crate::get_active_custom_provider_name_from(dir.path()).as_deref(),
        Some("copilot")
    );

    // The listing carried the session token and the login's headers.
    let seen = seen.lock().unwrap();
    let listing = seen.iter().find(|s| s.path.starts_with("/models")).unwrap();
    assert_eq!(listing.header("authorization"), Some("Bearer sess"));
    assert!(listing.header("editor-version").is_some());
}

/// A subscription check that fails stops the login before anything is
/// written.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_exchange_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _seen, server) = serve(vec![("/copilot_internal", 403, "{}".into())]);
    let spec = copilot_at(&base);

    let err = complete_account_login(dir.path(), spec, AccountTokens::new("g".into(), None, None))
        .await
        .unwrap_err();
    server.join().unwrap();

    assert!(oauth_refresh_requires_login(&err), "{err}");
    assert!(!credentials_json_path(dir.path()).exists());
    assert!(!dir.path().join("config.json").exists());
}

// ── Upsert and status ──────────────────────────────────────────

#[test]
fn upserting_twice_keeps_user_models_and_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    upsert_account_provider_in(dir.path(), copilot(), None).unwrap();
    crate::add_custom_provider_model_in(dir.path(), "copilot", "my-model").unwrap();
    upsert_account_provider_in(
        dir.path(),
        copilot(),
        Some("https://api.business.githubcopilot.com"),
    )
    .unwrap();

    let providers = list_custom_providers_from(dir.path());
    let entries: Vec<&CustomProviderInfo> =
        providers.iter().filter(|p| p.name == "copilot").collect();
    assert_eq!(entries.len(), 1);
    let entry = entries[0];
    assert_eq!(entry.base_url, "https://api.business.githubcopilot.com");
    assert!(entry.models.contains(&"my-model".to_string()));
    for model in copilot().provider.models {
        assert_eq!(
            entry.models.iter().filter(|m| m == model).count(),
            1,
            "{model}"
        );
    }
}

#[test]
fn the_chatgpt_upsert_is_the_existing_codex_upsert() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    upsert_account_provider_in(a.path(), codex_login(), None).unwrap();
    let models: Vec<String> = crate::OPENAI_OAUTH_PROVIDER_MODELS
        .iter()
        .map(|m| m.to_string())
        .collect();
    crate::upsert_openai_oauth_provider_in(b.path(), &models).unwrap();
    assert_eq!(
        read_json(&a.path().join("config.json")),
        read_json(&b.path().join("config.json"))
    );
}

#[test]
fn statuses_report_each_login_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    write_account_tokens(dir.path(), copilot(), &signed_in_copilot(Some(("t", 1)))).unwrap();
    upsert_account_provider_in(dir.path(), copilot(), None).unwrap();

    let statuses = account_login_statuses_in(dir.path()).unwrap();
    assert_eq!(statuses.len(), ACCOUNT_LOGINS.len());
    let codex = statuses.iter().find(|s| s.spec.is_codex()).unwrap();
    assert!(!codex.signed_in && !codex.provider_configured && !codex.active);
    let cop = statuses.iter().find(|s| s.spec.id == "copilot").unwrap();
    assert!(cop.signed_in && cop.provider_configured && cop.active);
    assert_eq!(cop.expires_at_ms, None, "the account token does not expire");
}

#[test]
fn setup_status_and_model_choices_know_every_login() {
    let dir = tempfile::tempdir().unwrap();
    upsert_account_provider_in(dir.path(), copilot(), None).unwrap();
    let status = provider_setup_status_in(dir.path());
    assert_eq!(
        status.providers[0].credential,
        ProviderSetupCredential::AccountLogin { login: "copilot" }
    );

    let info = list_custom_providers_from(dir.path()).remove(0);
    let choices = provider_model_choices(&info);
    for model in copilot().provider.models {
        assert!(choices.iter().any(|c| c.id == *model), "{model}");
    }
}

#[test]
fn account_headers_do_not_override_an_existing_header() {
    let mut headers = vec![("EDITOR-VERSION".to_string(), "mine".to_string())];
    merge_account_headers(copilot(), &mut headers);
    assert_eq!(
        headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("editor-version"))
            .count(),
        1
    );
    assert_eq!(headers.len(), copilot().request_headers.len());
}
