//! Wire types for bb-bridge/PROTOCOL.md (version 1).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// A line longer than this closes the connection.
pub const MAX_LINE: usize = 8 * 1024 * 1024;
/// Most document text a single response carries.
pub const MAX_TEXT: usize = 1024 * 1024;
/// Messages buffered per client before events are dropped.
pub const QUEUE_LEN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // some codes are only used by later steps
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    NotFound,
    VersionMismatch,
    BufferModified,
    NoLanguageServer,
    Timeout,
    TooLarge,
    Unsupported,
    Cancelled,
}

impl ErrorCode {
    pub fn code(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::NotFound => 1001,
            Self::VersionMismatch => 1002,
            Self::BufferModified => 1003,
            Self::NoLanguageServer => 1004,
            Self::Timeout => 1005,
            Self::TooLarge => 1006,
            Self::Unsupported => 1007,
            Self::Cancelled => 1008,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(field: &str, message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidParams,
            message: message.into(),
            data: Some(serde_json::json!({ "field": field })),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            ErrorCode::MethodNotFound,
            format!("method not found: {method}"),
        )
    }

    pub fn to_json(&self) -> Value {
        let mut err = serde_json::json!({
            "code": self.code.code(),
            "message": self.message,
        });
        if let Some(data) = &self.data {
            err["data"] = data.clone();
        }
        err
    }
}

pub type RpcResult = Result<Value, RpcError>;

/// Deserialize request params, mapping serde errors to `invalid_params`
/// with the offending field when serde names it.
pub fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = match params {
        Value::Null => Value::Object(Default::default()),
        params => params,
    };
    serde_json::from_value(params).map_err(|err| {
        let msg = err.to_string();
        let field = field_in_serde_error(&msg).unwrap_or("params").to_owned();
        RpcError::invalid_params(&field, msg)
    })
}

fn field_in_serde_error(msg: &str) -> Option<&str> {
    // serde_json phrases these as "missing field `x`" / "unknown field `x`" /
    // "invalid type: ..., expected ..." (no field). Pull out the backticked name.
    let start = msg.find('`')? + 1;
    let end = start + msg[start..].find('`')?;
    Some(&msg[start..end])
}

pub fn response(id: &Value, result: RpcResult) -> Value {
    match result {
        Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(err) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": err.to_json() }),
    }
}

pub fn notification(method: &str, params: impl Serialize) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

// --- shared shapes ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextRange {
    pub start: Position,
    pub end: Position,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentInfo {
    pub doc_id: usize,
    pub path: Option<String>,
    pub language_id: Option<String>,
    pub version: i32,
    pub modified: bool,
}

// --- bridge/hello ----------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub client_version: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub protocol: u32,
    pub helix_version: &'static str,
    pub pid: u32,
    pub cwd: String,
    pub workspace_root: String,
    pub capabilities: Vec<&'static str>,
}

// --- editor/state ----------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Visible {
    pub first_line: usize,
    pub last_line: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusedInfo {
    pub doc_id: usize,
    pub path: Option<String>,
    pub language_id: Option<String>,
    pub version: i32,
    pub modified: bool,
    pub selections: Vec<Range>,
    pub primary: usize,
    pub visible: Visible,
}

#[derive(Debug, Serialize)]
pub struct StateResult {
    pub mode: &'static str,
    pub focused: FocusedInfo,
    pub documents: Vec<DocumentInfo>,
}

// --- editor/open -----------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    #[default]
    None,
    Horizontal,
    Vertical,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenParams {
    pub path: String,
    pub line: Option<usize>,
    pub col: Option<usize>,
    pub end_line: Option<usize>,
    pub end_col: Option<usize>,
    #[serde(default)]
    pub split: Split,
    pub view_id: Option<u64>,
    #[serde(default = "default_true")]
    pub focus: bool,
    /// `"whenIdle"` or `"always"`; step 1 always moves at once.
    pub interrupt: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResult {
    pub doc_id: usize,
    pub path: String,
    pub version: i32,
    /// The pane showing the file; null when loaded in the background.
    pub view_id: Option<u64>,
    pub deferred: bool,
}

// --- editor/text -----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextParams {
    pub path: Option<String>,
    pub doc_id: Option<usize>,
    pub range: Option<Range>,
}

#[derive(Debug, Serialize)]
pub struct TextResult {
    pub path: Option<String>,
    pub version: Option<i32>,
    pub modified: bool,
    pub text: String,
    pub truncated: bool,
}

// --- editor/selection ------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionResult {
    pub path: Option<String>,
    pub doc_id: usize,
    pub language_id: Option<String>,
    pub version: i32,
    pub modified: bool,
    pub primary: usize,
    pub ranges: Vec<TextRange>,
    pub truncated: bool,
}

// --- editor/status ---------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Debug, Deserialize)]
pub struct StatusParams {
    pub message: String,
    #[serde(default)]
    pub severity: Severity,
}

// --- events ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SubscribeParams {
    pub events: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionEvent {
    pub path: Option<String>,
    pub doc_id: usize,
    pub version: i32,
    pub primary: usize,
    pub ranges: Vec<Range>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentEvent {
    pub doc_id: usize,
    pub path: Option<String>,
    pub language_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentChangedEvent {
    pub doc_id: usize,
    pub path: Option<String>,
    pub version: i32,
    pub modified: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSavedEvent {
    pub doc_id: usize,
    pub path: Option<String>,
    pub version: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusEvent {
    pub doc_id: usize,
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ModeEvent {
    pub mode: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SendKind {
    Send,
    Ask,
    Comment,
    Flush,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendEvent {
    pub kind: SendKind,
    pub note: String,
    pub path: Option<String>,
    pub doc_id: usize,
    pub language_id: Option<String>,
    pub version: i32,
    pub modified: bool,
    pub ranges: Vec<TextRange>,
    pub truncated: bool,
    pub send_now: bool,
}

#[derive(Debug, Serialize)]
pub struct OverflowEvent {
    pub dropped: u64,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_match_protocol() {
        let codes: Vec<i64> = [
            ErrorCode::ParseError,
            ErrorCode::InvalidRequest,
            ErrorCode::MethodNotFound,
            ErrorCode::InvalidParams,
            ErrorCode::NotFound,
            ErrorCode::VersionMismatch,
            ErrorCode::BufferModified,
            ErrorCode::NoLanguageServer,
            ErrorCode::Timeout,
            ErrorCode::TooLarge,
            ErrorCode::Unsupported,
            ErrorCode::Cancelled,
        ]
        .iter()
        .map(|c| c.code())
        .collect();
        assert_eq!(
            codes,
            [-32700, -32600, -32601, -32602, 1001, 1002, 1003, 1004, 1005, 1006, 1007, 1008]
        );
    }

    #[test]
    fn invalid_params_names_field() {
        let err = parse_params::<OpenParams>(serde_json::json!({ "line": 3 })).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert_eq!(err.data, Some(serde_json::json!({ "field": "path" })));

        let err =
            parse_params::<OpenParams>(serde_json::json!({ "path": "a", "split": "diagonal" }))
                .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn open_params_defaults() {
        let p: OpenParams = parse_params(serde_json::json!({ "path": "a.go" })).unwrap();
        assert!(p.focus);
        assert_eq!(p.split, Split::None);
        assert_eq!(p.line, None);
        let p: OpenParams =
            parse_params(serde_json::json!({ "path": "a.go", "focus": false, "split": "vertical", "line": 2, "col": 3, "endLine": 4, "endCol": 1 }))
                .unwrap();
        assert!(!p.focus);
        assert_eq!(p.split, Split::Vertical);
        assert_eq!(
            (p.line, p.col, p.end_line, p.end_col),
            (Some(2), Some(3), Some(4), Some(1))
        );
    }

    #[test]
    fn null_params_are_empty_object() {
        let p: HelloParams = parse_params(Value::Null).unwrap();
        assert!(p.client.is_none());
    }

    #[test]
    fn shapes_serialize_camel_case() {
        let ev = SendEvent {
            kind: SendKind::Comment,
            note: "n".into(),
            path: Some("/a".into()),
            doc_id: 3,
            language_id: Some("go".into()),
            version: 2,
            modified: false,
            ranges: vec![],
            truncated: false,
            send_now: false,
        };
        let v = serde_json::to_value(ev).unwrap();
        assert_eq!(v["kind"], "comment");
        assert_eq!(v["docId"], 3);
        assert_eq!(v["languageId"], "go");
        assert_eq!(v["sendNow"], false);

        let r = response(
            &serde_json::json!(7),
            Err(RpcError::invalid_params("path", "x")),
        );
        assert_eq!(r["error"]["code"], -32602);
        assert_eq!(r["error"]["data"]["field"], "path");
        assert_eq!(r["id"], 7);
    }
}
