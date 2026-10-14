//! The JSON-RPC envelope shapes the adapter speaks on the wire when it acts
//! as the agent side of the agent-client-protocol. The base protocol carries
//! its messages as JSON-RPC 2.0: a request has an id + method + params; a
//! response has the id back plus a result or an error; a notification has a
//! method + params and no id. The adapter routes by method name, then decodes
//! the typed params (Initialize, Prompt, RequestPermission, the
//! acpx/session/* ext verbs) which live in the crate's other modules. This
//! module owns the envelope only — the carrier is NDJSON (one message per
//! line), the same framing the base protocol already uses.
//!
//! The shapes track the wire envelope exactly (jsonrpc:"2.0", id as
//! null|number|string, result vs error discriminated by field presence) so a
//! stock client that speaks the base protocol drives the adapter without a
//! dialect mismatch. Typed params mirror in later commits; the envelope is
//! the foundation they ride on.

mod auth;
mod capabilities;
mod initialize;
mod permission;
mod prompt;
mod session;

pub use auth::{AuthMethod, AuthMethodAgent};
pub use capabilities::{
    AgentCapabilities, ClientCapabilities, FileSystemCapabilities, Implementation, McpCapabilities,
    PromptCapabilities, SessionCapabilities, SessionListCapabilities,
};
pub use initialize::{InitializeRequest, InitializeResponse};
pub use permission::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
pub use prompt::{PromptRequest, PromptResponse};
pub use session::{LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The JSON-RPC version marker. Always serializes as the string "2.0"; a
/// peer frame carrying any other version fails to deserialize so a mismatch
/// surfaces at the boundary, not mid-dispatch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonRpcVersion {
    #[default]
    #[serde(rename = "2.0")]
    V2,
}

/// A JSON-RPC request id. The base protocol allows null, integer, or string
/// (the spec's three id kinds); the adapter issues integer ids from its own
/// counter, but must accept all three on decode so a stock client's string
/// ids route correctly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcpRequestId {
    /// The null id. Rare on incoming requests; the adapter never issues one.
    Null,
    /// An integer id (the adapter's own outgoing shape).
    Number(i64),
    /// A string id (a stock client may issue these).
    Str(String),
}

/// A JSON-RPC request envelope: the caller's id, the method name, and the
/// optional typed params. The adapter dispatches by method, then decodes
/// params to the typed struct the method dictates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: AcpRequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC response: success carries a result, failure carries an error.
/// Discriminated by field presence (result vs error), matching the base
/// protocol's untagged shape — there is no separate success tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcpResponse {
    Result {
        jsonrpc: JsonRpcVersion,
        id: AcpRequestId,
        result: Value,
    },
    Error {
        jsonrpc: JsonRpcVersion,
        id: AcpRequestId,
        error: AcpError,
    },
}

/// A JSON-RPC notification: a method + params, no id. The session/update
/// stream rides notifications; the adapter never replies to one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpNotification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC error payload. The code is the typed ErrorCode; data carries
/// method-specific detail (or is absent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpError {
    pub code: AcpErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The standard JSON-RPC error codes plus the base-protocol extensions the
/// adapter may surface. Serializes as the raw integer (the wire form), so a
/// peer reading the code sees a number, not a variant tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "i32", into = "i32")]
pub enum AcpErrorCode {
    /// JSON parse error (-32700).
    ParseError,
    /// Invalid request envelope (-32600).
    InvalidRequest,
    /// Unknown method (-32601).
    MethodNotFound,
    /// Bad params for a known method (-32602).
    InvalidParams,
    /// Unexpected internal failure (-32603).
    InternalError,
    /// The session requires authentication (-32000).
    AuthRequired,
    /// A referenced resource (session, file) was not found (-32002).
    ResourceNotFound,
    /// Any other code the adapter does not model by name.
    Other(i32),
}

impl AcpErrorCode {
    /// The wire integer for this code.
    pub fn code(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::AuthRequired => -32000,
            Self::ResourceNotFound => -32002,
            Self::Other(c) => c,
        }
    }
}

impl From<i32> for AcpErrorCode {
    fn from(c: i32) -> Self {
        match c {
            -32700 => Self::ParseError,
            -32600 => Self::InvalidRequest,
            -32601 => Self::MethodNotFound,
            -32602 => Self::InvalidParams,
            -32603 => Self::InternalError,
            -32000 => Self::AuthRequired,
            -32002 => Self::ResourceNotFound,
            _ => Self::Other(c),
        }
    }
}

impl From<AcpErrorCode> for i32 {
    fn from(e: AcpErrorCode) -> i32 {
        e.code()
    }
}

/// The protocol version. The base protocol moved to numeric versions (V0=0,
/// V1=1, latest=V1); the wire form is a JSON number, not a string. A legacy
/// string peer is treated as V0 by the deserializer, but this copy
/// accepts only the numeric form — a string version fails at the boundary so
/// the adapter surfaces the mismatch rather than silently downgrading.
pub type ProtocolVersion = u16;

/// A free-form extension map the base protocol stows on every message under
/// the _meta key. The adapter places its typed AcpxCapabilities at the
/// _meta.acpx key of the initialize response so an acpx-aware client reads
/// it, while a stock client ignores the unknown field.
pub type Meta = serde_json::Map<String, Value>;

impl AcpRequest {
    pub fn ext_request(id: i64, ext_method: &str, params: Value) -> Self {
        Self::new(id, format!("_{ext_method}"), params)
    }
    /// Build an outgoing request with integer id and JSON params. The adapter
    /// uses this to issue its own requests (the reverse permission ask, the
    /// ext_method probes) when it acts as the JSON-RPC caller.
    pub fn new(id: i64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2,
            id: AcpRequestId::Number(id),
            method: method.into(),
            params: Some(params),
        }
    }
}

impl AcpResponse {
    /// Build a success response echoing the caller's id.
    pub fn ok(id: AcpRequestId, result: Value) -> Self {
        Self::Result {
            jsonrpc: JsonRpcVersion::V2,
            id,
            result,
        }
    }

    /// Build an error response echoing the caller's id.
    pub fn err(id: AcpRequestId, code: AcpErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            jsonrpc: JsonRpcVersion::V2,
            id,
            error: AcpError {
                code,
                message: message.into(),
                data: None,
            },
        }
    }
}

impl AcpNotification {
    /// Build an outgoing notification (session/update, ext_notification).
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2,
            method: method.into(),
            params: Some(params),
        }
    }
}

#[cfg(test)]
mod tests;
