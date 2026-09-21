use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u16 = 2;

/// One restricted synchronous script body and its JSON payload.
///
/// `code` is parsed as the body of a plain function. This protocol does not support
/// async functions, Promise results, dynamic imports, or ECMAScript modules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusScriptRequest {
    pub code: String,
    pub payload: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireRequest {
    pub protocol_version: u16,
    pub request: StatusScriptRequest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireResponse {
    pub protocol_version: u16,
    pub body: ResponseBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ResponseBody {
    Ok {
        value: Value,
        logs: Vec<String>,
    },
    Error {
        kind: ResponseErrorKind,
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseErrorKind {
    Script,
    OutputTooLarge,
}
