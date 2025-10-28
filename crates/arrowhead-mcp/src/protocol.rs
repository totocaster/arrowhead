//! JSON-RPC protocol implementation
//!
//! Core MCP protocol types and message handling helpers.

use std::fmt::{self, Display};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, error::Category};
use thiserror::Error;

/// JSON-RPC version string used across all payloads.
pub const JSON_RPC_VERSION: &str = "2.0";

/// JSON-RPC protocol version wrapper that guarantees `2.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Version;

impl Version {
    /// Create a new version instance.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(JSON_RPC_VERSION)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == JSON_RPC_VERSION {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported jsonrpc version '{value}'"
            )))
        }
    }
}

/// JSON-RPC request/response identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    /// Null identifier used for notifications or when the client picked null.
    Null,
    /// String identifier.
    Str(String),
    /// Signed integer identifier.
    Int(i64),
    /// Unsigned integer identifier.
    Uint(u64),
}

impl Default for Id {
    fn default() -> Self {
        Self::Null
    }
}

impl Serialize for Id {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Id::Null => serializer.serialize_none(),
            Id::Str(value) => serializer.serialize_str(value),
            Id::Int(value) => serializer.serialize_i64(*value),
            Id::Uint(value) => serializer.serialize_u64(*value),
        }
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Id;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON-RPC id (string, number, or null)")
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Id::Null)
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Id::Null)
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Id::Str(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Id::Str(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Id::Int(value))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Id::Uint(value))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl From<i64> for Id {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<i32> for Id {
    fn from(value: i32) -> Self {
        Self::Int(value as i64)
    }
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Self::Uint(value)
    }
}

impl From<u32> for Id {
    fn from(value: u32) -> Self {
        Self::Uint(value as u64)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for Id {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Null => write!(f, "null"),
            Id::Str(value) => write!(f, "{value}"),
            Id::Int(value) => write!(f, "{value}"),
            Id::Uint(value) => write!(f, "{value}"),
        }
    }
}

/// Wrapper around raw JSON parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Params(Value);

impl Params {
    /// Construct parameters from a serialisable payload.
    pub fn new<T: Serialize>(value: T) -> std::result::Result<Self, serde_json::Error> {
        serde_json::to_value(value).map(Self)
    }

    /// Attempt to deserialize the parameters into a strongly typed structure.
    pub fn deserialize<T: DeserializeOwned>(&self) -> std::result::Result<T, ProtocolError> {
        serde_json::from_value(self.0.clone())
            .map_err(|err| ProtocolError::invalid_params(err.to_string()))
    }

    /// Access the raw JSON representation.
    #[must_use]
    pub fn raw(&self) -> &Value {
        &self.0
    }
}

/// Strongly typed method name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Method(String);

impl Method {
    /// Create a method from any string-like input.
    pub fn new<T: Into<String>>(value: T) -> Self {
        Self(value.into())
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> From<T> for Method
where
    T: Into<String>,
{
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

/// JSON-RPC request carrying an identifier and parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version (always `2.0`).
    #[serde(default)]
    #[serde(rename = "jsonrpc")]
    pub version: Version,
    /// Request identifier provided by the client.
    pub id: Id,
    /// Fully qualified method name.
    pub method: Method,
    /// Raw JSON parameters.
    #[serde(default)]
    pub params: Params,
}

impl Request {
    /// Create a new request with the supplied identifier and parameters.
    pub fn new(id: impl Into<Id>, method: impl Into<Method>, params: Params) -> Self {
        Self {
            version: Version::new(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC notification (identical to a request but without an identifier).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// Protocol version (always `2.0`).
    #[serde(default)]
    #[serde(rename = "jsonrpc")]
    pub version: Version,
    /// Fully qualified method name.
    pub method: Method,
    /// Raw JSON parameters.
    #[serde(default)]
    pub params: Params,
}

impl Notification {
    /// Construct a new notification.
    pub fn new(method: impl Into<Method>, params: Params) -> Self {
        Self {
            version: Version::new(),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC success response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Success {
    /// Protocol version (always `2.0`).
    #[serde(default)]
    #[serde(rename = "jsonrpc")]
    pub version: Version,
    /// Request identifier echoed back to the client.
    pub id: Id,
    /// Result payload.
    pub result: Value,
}

impl Success {
    /// Create a success response.
    pub fn new(id: impl Into<Id>, result: Value) -> Self {
        Self {
            version: Version::new(),
            id: id.into(),
            result,
        }
    }
}

/// JSON-RPC failure response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    /// Protocol version (always `2.0`).
    #[serde(default)]
    #[serde(rename = "jsonrpc")]
    pub version: Version,
    /// Request identifier echoed back to the client.
    pub id: Id,
    /// Error descriptor.
    pub error: RpcError,
}

impl Failure {
    /// Create an error response.
    pub fn new(id: impl Into<Id>, error: RpcError) -> Self {
        Self {
            version: Version::new(),
            id: id.into(),
            error,
        }
    }
}

/// JSON-RPC response message (success or failure).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    /// Successful request.
    Success(Success),
    /// Failed request.
    Error(Failure),
}

impl Response {
    /// Access the request identifier associated with the response.
    #[must_use]
    pub fn id(&self) -> &Id {
        match self {
            Response::Success(Success { id, .. }) => id,
            Response::Error(Failure { id, .. }) => id,
        }
    }

    /// Create a success response.
    pub fn success(id: impl Into<Id>, result: Value) -> Self {
        Self::Success(Success::new(id, result))
    }

    /// Create an error response.
    pub fn error(id: impl Into<Id>, error: RpcError) -> Self {
        Self::Error(Failure::new(id, error))
    }
}

/// JSON-RPC message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// Request message.
    Request(Request),
    /// Notification message.
    Notification(Notification),
    /// Response message.
    Response(Response),
}

impl Message {
    /// Attempt to borrow the request variant.
    #[must_use]
    pub fn as_request(&self) -> Option<&Request> {
        match self {
            Message::Request(request) => Some(request),
            _ => None,
        }
    }

    /// Attempt to borrow the notification variant.
    #[must_use]
    pub fn as_notification(&self) -> Option<&Notification> {
        match self {
            Message::Notification(notification) => Some(notification),
            _ => None,
        }
    }

    /// Attempt to borrow the response variant.
    #[must_use]
    pub fn as_response(&self) -> Option<&Response> {
        match self {
            Message::Response(response) => Some(response),
            _ => None,
        }
    }
}

/// Incoming JSON-RPC frame — single message or batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    /// Single JSON-RPC message.
    Single(Message),
    /// Batch of JSON-RPC messages.
    Batch(Vec<Message>),
}

impl Incoming {
    /// Parse an incoming JSON document.
    pub fn parse_str(input: &str) -> std::result::Result<Self, ProtocolError> {
        let result: Result<Self, serde_json::Error> = serde_json::from_str(input);
        match result {
            Ok(message) => Ok(message),
            Err(error) => match error.classify() {
                Category::Syntax | Category::Io | Category::Eof => {
                    Err(ProtocolError::parse_error(error.to_string()))
                }
                _ => Err(ProtocolError::invalid_request(error.to_string())),
            },
        }
    }

    /// Convert raw JSON into an `Incoming` message.
    pub fn from_value(value: Value) -> std::result::Result<Self, ProtocolError> {
        serde_json::from_value(value).map_err(|err| match err.classify() {
            Category::Syntax | Category::Io | Category::Eof => {
                ProtocolError::parse_error(err.to_string())
            }
            _ => ProtocolError::invalid_request(err.to_string()),
        })
    }
}

/// MCP-aware JSON-RPC error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    /// Invalid JSON received by the server.
    ParseError = -32700,
    /// The JSON sent is not a valid Request object.
    InvalidRequest = -32600,
    /// The method does not exist / is not available.
    MethodNotFound = -32601,
    /// Invalid method parameter(s).
    InvalidParams = -32602,
    /// Internal JSON-RPC error.
    InternalError = -32603,
    /// Generic server error (implementation-defined).
    ServerError = -32000,
    /// Transport or service dependency unavailable.
    ServiceUnavailable = -32002,
    /// Feature is disabled (e.g., semantic search when embeddings are not initialised).
    ToolDisabled = -32003,
    /// Request timed out before completion.
    Timeout = -32004,
    /// Request exceeded resource limits.
    RateLimited = -32005,
}

impl ErrorCode {
    /// Return the integer code.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// JSON-RPC error structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Optional implementation-specific data payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Construct a new error from the supplied code and message.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    /// Attach additional error context in the `data` field.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// High-level protocol error representation used by internal logic.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// Invalid JSON payload that could not be parsed.
    #[error("parse error: {message}")]
    ParseError {
        /// Error description.
        message: String,
    },
    /// Payload failed schema validation.
    #[error("invalid request: {message}")]
    InvalidRequest {
        /// Error description.
        message: String,
    },
    /// Requested method is unknown.
    #[error("method not found: {method}")]
    MethodNotFound {
        /// Method name that could not be resolved.
        method: String,
    },
    /// Method parameters could not be decoded.
    #[error("invalid params: {message}")]
    InvalidParams {
        /// Error description.
        message: String,
    },
    /// Unhandled error occurred inside the request handler.
    #[error("internal error: {message}")]
    InternalError {
        /// Error description.
        message: String,
    },
    /// Custom error code with optional JSON data payload.
    #[error("{code:?}: {message}")]
    Custom {
        /// Custom error code.
        code: ErrorCode,
        /// Error description.
        message: String,
        /// Optional error data.
        data: Option<Value>,
    },
}

impl ProtocolError {
    /// Convert into a `RpcError` ready for transmission.
    #[must_use]
    pub fn into_rpc(self) -> RpcError {
        match self {
            ProtocolError::ParseError { message } => RpcError::new(ErrorCode::ParseError, message),
            ProtocolError::InvalidRequest { message } => {
                RpcError::new(ErrorCode::InvalidRequest, message)
            }
            ProtocolError::MethodNotFound { method } => RpcError::new(
                ErrorCode::MethodNotFound,
                format!("unknown method '{method}'"),
            ),
            ProtocolError::InvalidParams { message } => {
                RpcError::new(ErrorCode::InvalidParams, message)
            }
            ProtocolError::InternalError { message } => {
                RpcError::new(ErrorCode::InternalError, message)
            }
            ProtocolError::Custom {
                code,
                message,
                data,
            } => {
                let base = RpcError::new(code, message);
                if let Some(data) = data {
                    base.with_data(data)
                } else {
                    base
                }
            }
        }
    }

    /// Helper for parse errors.
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::ParseError {
            message: message.into(),
        }
    }

    /// Helper for invalid request errors.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    /// Helper for invalid parameter errors.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::InvalidParams {
            message: message.into(),
        }
    }

    /// Helper for internal errors.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::InternalError {
            message: message.into(),
        }
    }

    /// Helper for custom errors.
    pub fn custom(code: ErrorCode, message: impl Into<String>, data: Option<Value>) -> Self {
        Self::Custom {
            code,
            message: message.into(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_request() {
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "method": "mcp.echo",
            "params": {"message": "hello"}
        }"#;

        let msg = Incoming::parse_str(raw).expect("parse request");
        let request = match msg {
            Incoming::Single(Message::Request(req)) => req,
            _ => panic!("expected single request"),
        };

        assert!(
            matches!(request.id, Id::Int(1) | Id::Uint(1)),
            "unexpected request id variant: {:?}",
            request.id
        );
        assert_eq!(request.method.as_str(), "mcp.echo");
        let params: serde_json::Map<String, Value> =
            request.params.deserialize().expect("deserialize params");
        assert_eq!(params.get("message").unwrap(), "hello");
    }

    #[test]
    fn serialize_success_response() {
        let response = Response::success(42, serde_json::json!({"ok": true}));
        let json = serde_json::to_string(&response).expect("serialize response");
        let expected = serde_json::json!({"jsonrpc": "2.0", "id": 42, "result": {"ok": true}});
        let decoded: Value = serde_json::from_str(&json).expect("decode json");
        assert_eq!(decoded, expected);
    }

    #[test]
    fn params_decode_failure_maps_to_protocol_error() {
        let params = Params::new(serde_json::json!({"limit": "oops"})).expect("params");
        let err = params.deserialize::<TestParams>().expect_err("should fail");
        match err {
            ProtocolError::InvalidParams { message } => {
                assert!(
                    message.contains("invalid type"),
                    "unexpected error message: {message}"
                );
            }
            _ => panic!("expected invalid params error"),
        }
    }

    #[test]
    fn error_code_values_match_spec() {
        assert_eq!(ErrorCode::ParseError.code(), -32700);
        assert_eq!(ErrorCode::InvalidRequest.code(), -32600);
        assert_eq!(ErrorCode::MethodNotFound.code(), -32601);
        assert_eq!(ErrorCode::InvalidParams.code(), -32602);
        assert_eq!(ErrorCode::InternalError.code(), -32603);
        assert_eq!(ErrorCode::ServiceUnavailable.code(), -32002);
    }

    #[derive(Debug, Deserialize)]
    struct TestParams {
        #[allow(dead_code)]
        message: String,
        #[allow(dead_code)]
        limit: u32,
    }

    #[test]
    fn protocol_error_into_rpc() {
        let err = ProtocolError::invalid_request("missing method");
        let rpc = err.into_rpc();
        assert_eq!(rpc.code, ErrorCode::InvalidRequest.code());
        assert_eq!(rpc.message, "missing method");
        assert!(rpc.data.is_none());
    }
}
