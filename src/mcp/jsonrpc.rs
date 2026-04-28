//! JSON-RPC 2.0 wire types for the MCP server.
//!
//! Hand-rolled rather than pulling a JSON-RPC crate. Two reasons: (1) the
//! framing edge cases (oversize, malformed, EOF mid-message) are easier to
//! audit when we own them, and (2) we want full control over how
//! request `id` is preserved across the parse → dispatch → reply path so
//! that a request whose params fail validation still echoes the right id.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 protocol version. Always the literal `"2.0"`.
pub const JSONRPC_VERSION: &str = "2.0";

/// Request id. Per JSON-RPC 2.0, may be a number, string, or null.
/// Notifications omit the id entirely.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
    Null,
}

/// A parsed inbound message. Either an ordinary request (has `id`),
/// a notification (no `id`), or a response we received (rare on the
/// server side; clients send `roots/list` etc., but in v0.1 we only
/// act as a server, so this exists mainly for completeness).
#[derive(Debug)]
pub enum Inbound {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC 2.0 error codes plus the MCP-reserved range.
pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

impl Response {
    pub fn success(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Id, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(ErrorObject {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    pub fn error_with_data(id: Id, code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(ErrorObject {
                code,
                message: message.into(),
                data: Some(data),
            }),
        }
    }
}

/// Parse a single JSON-RPC frame into one of three message shapes.
///
/// We deliberately avoid `#[serde(untagged)]` on a single enum because
/// untagged dispatch on optional fields is order-sensitive in serde and
/// hides which variant actually matched on parse failures.
pub fn parse_inbound(line: &[u8]) -> Result<Inbound, ParseError> {
    let value: Value = serde_json::from_slice(line).map_err(ParseError::InvalidJson)?;

    let obj = value.as_object().ok_or(ParseError::NotAnObject)?;

    let jsonrpc = obj
        .get("jsonrpc")
        .and_then(Value::as_str)
        .ok_or(ParseError::MissingJsonrpc)?;
    if jsonrpc != JSONRPC_VERSION {
        return Err(ParseError::WrongVersion(jsonrpc.to_string()));
    }

    let has_method = obj.contains_key("method");
    let has_id = obj.contains_key("id");
    let has_result_or_error = obj.contains_key("result") || obj.contains_key("error");

    if has_method && has_id {
        let req: Request = serde_json::from_value(value).map_err(ParseError::InvalidJson)?;
        Ok(Inbound::Request(req))
    } else if has_method && !has_id {
        let n: Notification = serde_json::from_value(value).map_err(ParseError::InvalidJson)?;
        Ok(Inbound::Notification(n))
    } else if has_result_or_error && has_id {
        let r: Response = serde_json::from_value(value).map_err(ParseError::InvalidJson)?;
        Ok(Inbound::Response(r))
    } else {
        Err(ParseError::Unrecognized)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(serde_json::Error),
    #[error("frame is not a JSON object")]
    NotAnObject,
    #[error("missing `jsonrpc` field")]
    MissingJsonrpc,
    #[error("unsupported jsonrpc version: {0}")]
    WrongVersion(String),
    #[error("frame is neither request, notification, nor response")]
    Unrecognized,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_request() {
        let bytes = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let parsed = parse_inbound(bytes).unwrap();
        match parsed {
            Inbound::Request(r) => {
                assert_eq!(r.method, "tools/list");
                assert_eq!(r.id, Id::Number(1));
                assert!(r.params.is_none());
            }
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn parse_notification() {
        let bytes = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let parsed = parse_inbound(bytes).unwrap();
        assert!(matches!(parsed, Inbound::Notification(_)));
    }

    #[test]
    fn parse_string_id() {
        let bytes = br#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#;
        match parse_inbound(bytes).unwrap() {
            Inbound::Request(r) => assert_eq!(r.id, Id::String("abc".into())),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_response() {
        let bytes = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        assert!(matches!(
            parse_inbound(bytes).unwrap(),
            Inbound::Response(_)
        ));
    }

    #[test]
    fn rejects_wrong_version() {
        let bytes = br#"{"jsonrpc":"1.0","id":1,"method":"x"}"#;
        assert!(matches!(
            parse_inbound(bytes),
            Err(ParseError::WrongVersion(_))
        ));
    }

    #[test]
    fn rejects_non_object() {
        assert!(matches!(parse_inbound(b"[]"), Err(ParseError::NotAnObject)));
        assert!(matches!(parse_inbound(b"42"), Err(ParseError::NotAnObject)));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(matches!(
            parse_inbound(b"{not json"),
            Err(ParseError::InvalidJson(_))
        ));
    }

    #[test]
    fn rejects_missing_jsonrpc() {
        let bytes = br#"{"id":1,"method":"x"}"#;
        assert!(matches!(
            parse_inbound(bytes),
            Err(ParseError::MissingJsonrpc)
        ));
    }

    #[test]
    fn response_success_round_trips() {
        let r = Response::success(Id::Number(7), json!({"ok": true}));
        let s = serde_json::to_string(&r).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["ok"], true);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn response_error_round_trips() {
        let r = Response::error(Id::String("x".into()), error_codes::METHOD_NOT_FOUND, "no");
        let s = serde_json::to_string(&r).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], "x");
        assert_eq!(v["error"]["code"], -32601);
        assert!(v.get("result").is_none());
    }
}
