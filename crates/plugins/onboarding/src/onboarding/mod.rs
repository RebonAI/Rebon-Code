//! # `onboarding` — the wizard and login decisions, with no IO
//!
//! This module is the pure-decision half of the terminal UI first-run
//! experience. It covers:
//!
//! * The pure half of the OpenAI (Codex) OAuth login: opening a browser
//! to a PKCE-protected authorization URL, either auto-capturing the code
//! via a localhost callback OR prompting the user to paste it, and
//! parsing the token-exchange response. Modeled by [`oauth_url`],
//! [`pkce`], [`code_parser`], and [`token_response`]; the IO half is
//! [`crate::oauth`].
//! * PKCE material — the challenge as a pure
//! SHA-256 + base64url function, the state and verifier with random
//! bytes injected by the caller. Modeled by [`pkce`].
//! * The OpenAI authorize-URL builder. Modeled by
//! [`oauth_url::build_openai_authorize_url`].
//! * The prod constants, pinned as
//! `pub const`s in [`oauth_url`].
//!
//! Every module pins its behaviour
//! with a comprehensive test table.
//!
//! ## Why this crate has a higher security bar
//!
//! `onboarding` owns the **first-time login flow** for Rebon: it's
//! how a fresh install obtains the user's OAuth tokens. Every
//! decision rule in `pkce`, `oauth_url`, `code_parser`, and
//! `token_response` is a
//! security-critical surface. Whenever the Rust deviates from these
//! rules the deviation MUST be intentional and pinned
//! by an explicit test.
//!
//! Pinned OAuth-decision rules (each with positive + negative + edge
//! tests in the corresponding module):
//!
//! 1. **PKCE challenge is RFC 7636 S256**, computed as
//! `base64url(sha256(verifier))` with **no `=` padding** and the
//! URL-safe alphabet (`-`/`_` instead of `+`/`/`). See
//! [`pkce::code_challenge_s256`].
//! 2. **PKCE verifier is base64url-encoded random bytes** (32 bytes →
//! 43 chars). The verifier text bytes (NOT the raw 32 bytes) are
//! fed into SHA-256. See
//! [`pkce::encode_verifier`] and [`pkce::code_challenge_s256`].
//! 3. **The `state` parameter is base64url-encoded random bytes** of
//! the same length and format as the verifier. State and verifier
//! are NEVER the same value (they MUST be drawn from independent
//! randomness). See [`pkce::encode_state`].
//! 4. **The authorize URL must include `code_challenge_method=S256`,
//! not `plain`.** Pinned in [`oauth_url::build_openai_authorize_url`].
//! 5. **The `state` parameter is required.** A built URL without
//! state is rejected by [`oauth_url::AuthorizeUrlError::EmptyState`].
//! 6. **`scope=` for the OpenAI flow is the literal
//! `openid profile email offline_access`.** Pinned in
//! [`oauth_url::OPENAI_SCOPES`].
//! 7. **The pasted code format is `<authorization_code>#<state>`** with a
//! literal `#` separator (NOT `&` or `?`). Both halves must be
//! non-empty. See [`code_parser::parse_pasted_code`].
//! 8. **The token endpoint expects `expires_in` in seconds, not
//! milliseconds.** When converting to a Unix timestamp, multiply
//! by 1000. See [`token_response::expires_at_from_response`].
//!
//! ## What is in this crate
//!
//! * [`pkce`] — PKCE crypto. `code_challenge_s256(verifier_text)` is
//! pure (SHA-256 + base64url, no padding). `encode_verifier` /
//! `encode_state` take 32 random bytes supplied by the caller.
//! * [`oauth_url`] — `build_openai_authorize_url`. Pins every
//! parameter encoding rule. The prod constants (client id, redirect
//! URL, scopes) live as `pub const` so the consumer can pin them too.
//! * [`code_parser`] — `parse_pasted_code` (the
//! `<authorization_code>#<state>` splitter) and `parse_callback_url`
//! (extracts `code` and `state` from a callback URL).
//! * [`token_response`] — `parse_token_exchange_response` (typed
//! struct from the JSON) and `expires_at_from_response`. Hand-rolled
//! parser so we don't have to take a serde dep — keeps the crate
//! dependency-light.
//! * [`dialog_state`] — the wizard's step, pane and field vocabulary,
//! and the login-method picker's options and row grouping.
//! * [`text_field`] — the single-line text input the wizard's forms use.
//!
//! ## What lives elsewhere
//!
//! * **The browser launch, the localhost callback listener, the
//! token-exchange HTTP call and the credential write.** These are
//! [`crate::oauth`]; this module never performs IO.
//! * **Wall-clock time and randomness.** Taken as parameters (a `now` value,
//! 32 random bytes) so tests drive them deterministically.
//! * **Rendering and key dispatch.** The terminal dialog draws the states
//! this module names and drives the transitions.

pub mod code_parser;
pub mod dialog_state;
pub mod oauth_url;
pub mod pkce;
pub mod text_field;
pub mod token_response;

pub use code_parser::{
    parse_callback_url, parse_pasted_code, CodeParseError, ParsedCallback, ParsedPastedCode,
};
pub use dialog_state::{
    actionable_login_options, grouped_login_pane_rows, login_option_group, login_row_for_focus,
    preset_window, LoginMethodOption, LoginPaneRow, ProviderField, ProviderFormMode, SetupPane,
    Step, PRESET_ROWS_VISIBLE,
};
pub use oauth_url::{
    build_openai_authorize_url, AuthorizeUrlError, OpenAIAuthInputs, OPENAI_AUTHORIZE_URL,
    OPENAI_CALLBACK_PATH, OPENAI_CLIENT_ID, OPENAI_REDIRECT_PORT, OPENAI_REDIRECT_URI,
    OPENAI_SCOPES, OPENAI_TOKEN_URL,
};
pub use pkce::{code_challenge_s256, encode_state, encode_verifier};
pub use text_field::{
    next_char_boundary, normalize_single_line_paste, prev_char_boundary, TextField,
};
pub use token_response::{
    expires_at_from_response, parse_token_exchange_response, TokenExchangeError,
    TokenExchangeResponse,
};
