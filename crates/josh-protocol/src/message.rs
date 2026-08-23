use std::fmt;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::strict_json;

pub const WIRE_PROTOCOL: &str = "josh/1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    Io(String),
    UnexpectedEof,
    HeaderLineTooLong,
    InvalidHeader,
    InvalidLength,
    FrameTooLarge,
    InvalidUtf8,
    InvalidJson(String),
    InvalidMessage(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "protocol I/O failed: {error}"),
            Self::UnexpectedEof => formatter.write_str("protocol frame ended early"),
            Self::HeaderLineTooLong => formatter.write_str("protocol header line is too long"),
            Self::InvalidHeader => formatter.write_str("protocol frame header is invalid"),
            Self::InvalidLength => formatter.write_str("protocol content length is invalid"),
            Self::FrameTooLarge => formatter.write_str("protocol frame exceeds its limit"),
            Self::InvalidUtf8 => formatter.write_str("protocol body is not UTF-8"),
            Self::InvalidJson(error) => {
                write!(formatter, "protocol body is not strict JSON: {error}")
            }
            Self::InvalidMessage(error) => {
                write!(formatter, "protocol message is invalid: {error}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<std::io::Error> for ProtocolError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WireErrorCode {
    #[serde(rename = "request.invalid")]
    RequestInvalid,
    #[serde(rename = "request.method_not_found")]
    RequestMethodNotFound,
    #[serde(rename = "request.invalid_state")]
    RequestInvalidState,
    #[serde(rename = "request.limit")]
    RequestLimit,
    #[serde(rename = "request.cancelled")]
    RequestCancelled,
    #[serde(rename = "catalog.invalid")]
    CatalogInvalid,
    #[serde(rename = "catalog.mismatch")]
    CatalogMismatch,
    #[serde(rename = "program.invalid")]
    ProgramInvalid,
    #[serde(rename = "program.unsatisfied")]
    ProgramUnsatisfied,
    #[serde(rename = "execution.duplicate")]
    ExecutionDuplicate,
    #[serde(rename = "execution.failed")]
    ExecutionFailed,
    #[serde(rename = "tool.denied")]
    ToolDenied,
    #[serde(rename = "tool.unavailable")]
    ToolUnavailable,
    #[serde(rename = "agent.denied")]
    AgentDenied,
    #[serde(rename = "agent.unavailable")]
    AgentUnavailable,
    #[serde(rename = "model.denied")]
    ModelDenied,
    #[serde(rename = "model.unavailable")]
    ModelUnavailable,
    #[serde(rename = "user.denied")]
    UserDenied,
    #[serde(rename = "user.unavailable")]
    UserUnavailable,
    #[serde(rename = "sub_agent.denied")]
    SubAgentDenied,
    #[serde(rename = "sub_agent.unavailable")]
    SubAgentUnavailable,
    #[serde(rename = "replay.diverged")]
    ReplayDiverged,
    #[serde(rename = "permission.unavailable")]
    PermissionUnavailable,
    #[serde(rename = "protocol.violation")]
    ProtocolViolation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireError {
    pub code: WireErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl WireError {
    /// Validates bounded safe error text.
    ///
    /// # Errors
    ///
    /// Returns an error when the message exceeds the protocol limit.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.message.len() > 1_024 {
            return Err(invalid("wire error message exceeds 1,024 UTF-8 bytes"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WireMessage {
    Request {
        id: String,
        method: String,
        params: Value,
    },
    Response {
        id: String,
        result: Option<Value>,
        error: Option<WireError>,
    },
    Notification {
        method: String,
        params: Value,
    },
    Cancel {
        id: String,
        reason: Option<String>,
    },
}

impl WireMessage {
    /// Validates all structural scalar constraints on the message.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid ID, method, reason, response, or wire error.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Request { id, method, .. } => {
                validate_id(id)?;
                validate_method(method)
            }
            Self::Response { id, result, error } => {
                validate_id(id)?;
                if result.is_some() == error.is_some() {
                    return Err(invalid(
                        "response must contain exactly one of result or error",
                    ));
                }
                if let Some(error) = error {
                    error.validate()?;
                }
                Ok(())
            }
            Self::Notification { method, .. } => validate_method(method),
            Self::Cancel { id, reason } => {
                validate_id(id)?;
                if let Some(reason) = reason {
                    validate_reason(reason)?;
                }
                Ok(())
            }
        }
    }

    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } | Self::Cancel { id, .. } => {
                Some(id)
            }
            Self::Notification { .. } => None,
        }
    }
}

impl Serialize for WireMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Request { id, method, params } => {
                let mut object = serializer.serialize_struct("WireMessage", 5)?;
                object.serialize_field("protocol", WIRE_PROTOCOL)?;
                object.serialize_field("kind", "request")?;
                object.serialize_field("id", id)?;
                object.serialize_field("method", method)?;
                object.serialize_field("params", params)?;
                object.end()
            }
            Self::Response { id, result, error } => {
                let mut object = serializer.serialize_struct("WireMessage", 4)?;
                object.serialize_field("protocol", WIRE_PROTOCOL)?;
                object.serialize_field("kind", "response")?;
                object.serialize_field("id", id)?;
                if let Some(result) = result {
                    object.serialize_field("result", result)?;
                } else if let Some(error) = error {
                    object.serialize_field("error", error)?;
                }
                object.end()
            }
            Self::Notification { method, params } => {
                let mut object = serializer.serialize_struct("WireMessage", 4)?;
                object.serialize_field("protocol", WIRE_PROTOCOL)?;
                object.serialize_field("kind", "notification")?;
                object.serialize_field("method", method)?;
                object.serialize_field("params", params)?;
                object.end()
            }
            Self::Cancel { id, reason } => {
                let mut object = serializer
                    .serialize_struct("WireMessage", if reason.is_some() { 4 } else { 3 })?;
                object.serialize_field("protocol", WIRE_PROTOCOL)?;
                object.serialize_field("kind", "cancel")?;
                object.serialize_field("id", id)?;
                if let Some(reason) = reason {
                    object.serialize_field("reason", reason)?;
                }
                object.end()
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEnvelope {
    protocol: String,
    kind: RequestKind,
    id: String,
    method: String,
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultEnvelope {
    protocol: String,
    kind: ResponseKind,
    id: String,
    result: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorEnvelope {
    protocol: String,
    kind: ResponseKind,
    id: String,
    error: WireError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationEnvelope {
    protocol: String,
    kind: NotificationKind,
    method: String,
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelEnvelope {
    protocol: String,
    kind: CancelKind,
    id: String,
    reason: Option<String>,
}

macro_rules! unit_string_enum {
    ($name:ident, $value:literal) => {
        #[derive(Deserialize)]
        enum $name {
            #[serde(rename = $value)]
            Value,
        }
    };
}

unit_string_enum!(RequestKind, "request");
unit_string_enum!(ResponseKind, "response");
unit_string_enum!(NotificationKind, "notification");
unit_string_enum!(CancelKind, "cancel");

/// Decodes one strict, exact-shape wire message.
///
/// # Errors
///
/// Returns an error for invalid UTF-8, JSON, duplicate keys, or message fields.
pub fn decode_message(input: &[u8]) -> Result<WireMessage, ProtocolError> {
    let value = decode_value(input)?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("message must be a JSON object"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("message kind is missing or invalid"))?;
    let message = match kind {
        "request" => {
            let envelope: RequestEnvelope = from_value(value)?;
            check_protocol(&envelope.protocol)?;
            let _ = envelope.kind;
            WireMessage::Request {
                id: envelope.id,
                method: envelope.method,
                params: envelope.params,
            }
        }
        "response" if object.contains_key("result") && !object.contains_key("error") => {
            let envelope: ResultEnvelope = from_value(value)?;
            check_protocol(&envelope.protocol)?;
            let _ = envelope.kind;
            WireMessage::Response {
                id: envelope.id,
                result: Some(envelope.result),
                error: None,
            }
        }
        "response" if object.contains_key("error") && !object.contains_key("result") => {
            let envelope: ErrorEnvelope = from_value(value)?;
            check_protocol(&envelope.protocol)?;
            let _ = envelope.kind;
            WireMessage::Response {
                id: envelope.id,
                result: None,
                error: Some(envelope.error),
            }
        }
        "response" => {
            return Err(invalid(
                "response must contain exactly one of result or error",
            ));
        }
        "notification" => {
            let envelope: NotificationEnvelope = from_value(value)?;
            check_protocol(&envelope.protocol)?;
            let _ = envelope.kind;
            WireMessage::Notification {
                method: envelope.method,
                params: envelope.params,
            }
        }
        "cancel" => {
            let envelope: CancelEnvelope = from_value(value)?;
            check_protocol(&envelope.protocol)?;
            let _ = envelope.kind;
            WireMessage::Cancel {
                id: envelope.id,
                reason: envelope.reason,
            }
        }
        _ => return Err(invalid("message kind is unknown")),
    };
    message.validate()?;
    Ok(message)
}

/// Decodes one strict JSON value and rejects duplicate keys at every depth.
///
/// # Errors
///
/// Returns an error for invalid UTF-8, JSON, duplicate keys, or trailing bytes.
pub fn decode_value(input: &[u8]) -> Result<Value, ProtocolError> {
    strict_json::parse(input)
}

/// Encodes one validated wire message as UTF-8 JSON.
///
/// # Errors
///
/// Returns an error when the message is invalid or serialization fails.
pub fn encode_message(message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    message.validate()?;
    serde_json::to_vec(message).map_err(|error| ProtocolError::InvalidJson(error.to_string()))
}

/// Validates an opaque wire request ID.
///
/// # Errors
///
/// Returns an error when the ID is empty, too long, non-ASCII, or whitespace-bearing.
pub fn validate_id(id: &str) -> Result<(), ProtocolError> {
    if !(1..=64).contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid(
            "ID must be 1 through 64 non-whitespace printable ASCII bytes",
        ));
    }
    Ok(())
}

/// Validates a canonical wire method.
///
/// # Errors
///
/// Returns an error when the method violates its bounded lower-case ASCII grammar.
pub fn validate_method(method: &str) -> Result<(), ProtocolError> {
    if !(1..=64).contains(&method.len())
        || method.starts_with('/')
        || method.ends_with('/')
        || method.split('/').any(str::is_empty)
        || !method.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'/')
        })
    {
        return Err(invalid("method is not canonical lower-case ASCII"));
    }
    Ok(())
}

/// Validates bounded optional reason text.
///
/// # Errors
///
/// Returns an error when the reason exceeds 1,024 UTF-8 bytes.
pub fn validate_reason(reason: &str) -> Result<(), ProtocolError> {
    if reason.len() > 1_024 {
        return Err(invalid("reason exceeds 1,024 UTF-8 bytes"));
    }
    Ok(())
}

fn check_protocol(protocol: &str) -> Result<(), ProtocolError> {
    if protocol != WIRE_PROTOCOL {
        return Err(invalid("protocol must be josh/1"));
    }
    Ok(())
}

fn from_value<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(value).map_err(|error| invalid(error.to_string()))
}

pub(crate) fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::InvalidMessage(message.into())
}
