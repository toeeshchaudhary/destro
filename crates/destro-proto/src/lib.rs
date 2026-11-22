//! The Destro wire vocabulary.
//!
//! Destro speaks the DeepSeek Harness SDK protocol: newline-delimited JSON-RPC
//! 2.0, one frame per line, over any byte stream. The request and notification
//! names here are wire-stable because existing clients (the TUI, the Python SDK,
//! `dsh` itself) already speak them. Everything under [`ext`] is Destro's own
//! addition: a control plane for inspecting and steering individual agents,
//! which the upstream protocol does not expose.
//!
//! This crate is pure data: no I/O, no runtime, no plugins.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod event;
pub use event::{SessionEvent, Usage};

/// Client-to-server requests from the dsh SDK protocol.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_QUERY: &str = "session/query";
    pub const SESSION_RESUME: &str = "session/resume";
    pub const SESSION_WAIT: &str = "session/wait";
    pub const SESSION_WD_GET: &str = "session/working-directory/get";
    pub const SESSION_WD_SET: &str = "session/working-directory/set";
    pub const SHUTDOWN: &str = "shutdown";
}

/// Server-to-client requests and notifications from the dsh SDK protocol.
pub mod notify {
    pub const APPROVAL_REQUEST: &str = "approval/request";
    pub const USER_QUESTION_ASK: &str = "user-question/ask";
    pub const USER_QUESTION_CANCELLED: &str = "user-question.cancelled";
    pub const SESSION_EVENT: &str = "session.event";
    pub const SESSION_STATUS: &str = "session.status";
    pub const SESSION_QUERY_COMPLETED: &str = "session.query.completed";
    pub const SUBAGENT_STARTED: &str = "subagent.started";
    pub const SUBAGENT_FINISHED: &str = "subagent.finished";
}

/// Destro control-plane extensions.
///
/// These keep the upstream call shape (`{id, method, params}`) so a client that
/// already speaks the SDK protocol can call them without a second transport.
pub mod ext {
    /// `agent/list` — `{ parent?: string, status?: string }` → `{ agents: AgentState[] }`.
    pub const AGENT_LIST: &str = "agent/list";
    /// `agent/state` — `{ agentId: string }` → `AgentState`.
    pub const AGENT_STATE: &str = "agent/state";
    /// `agent/spawn` — `SpawnParams` → `{ agentId, sessionId }`.
    pub const AGENT_SPAWN: &str = "agent/spawn";
    /// `agent/send` — `{ agentId, text, mode?: "queue" | "inject" }` → `{}`.
    pub const AGENT_SEND: &str = "agent/send";
    /// `agent/interrupt` — `{ agentId }` → `{}`.
    pub const AGENT_INTERRUPT: &str = "agent/interrupt";
    /// `agent/kill` — `{ agentId, tree?: bool }` → `{ killed: number }`.
    pub const AGENT_KILL: &str = "agent/kill";
    /// `agent/wait` — `{ agentId }` → `{ status, lastMessage? }`.
    pub const AGENT_WAIT: &str = "agent/wait";
    /// `agent/models` — `{}` → `{ models: [{ id, provider }] }`.
    pub const AGENT_MODELS: &str = "agent/models";
    /// `agent/transcript` — `{ agentId, limit?: number }` → `{ events: SessionEventValue[] }`.
    pub const AGENT_TRANSCRIPT: &str = "agent/transcript";
}

/// One decoded JSON-RPC frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Value,
    },
    Error {
        id: Value,
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

impl Frame {
    /// Decode one line. Returns `None` for malformed or empty lines, matching the
    /// protocol's "malformed lines are ignored" rule.
    pub fn parse(line: &str) -> Option<Frame> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        let obj = v.as_object()?;
        let id = obj.get("id").cloned().filter(|i| !i.is_null());
        let method = obj
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => Some(Frame::Request { id, method, params }),
            (Some(method), None) => Some(Frame::Notification { method, params }),
            (None, Some(id)) => match obj.get("error") {
                Some(err) => Some(Frame::Error {
                    id,
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                    data: err.get("data").cloned(),
                }),
                None => Some(Frame::Response {
                    id,
                    result: obj.get("result").cloned().unwrap_or(Value::Null),
                }),
            },
            (None, None) => None,
        }
    }

    pub fn to_line(&self) -> String {
        self.to_value().to_string()
    }

    pub fn to_value(&self) -> Value {
        match self {
            Frame::Request { id, method, params } => {
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            }
            Frame::Notification { method, params } => {
                json!({"jsonrpc": "2.0", "method": method, "params": params})
            }
            Frame::Response { id, result } => {
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            Frame::Error {
                id,
                code,
                message,
                data,
            } => {
                let mut err = json!({"code": code, "message": message});
                if let Some(data) = data {
                    err["data"] = data.clone();
                }
                json!({"jsonrpc": "2.0", "id": id, "error": err})
            }
        }
    }

    pub fn request(id: impl Into<Value>, method: &str, params: Value) -> Frame {
        Frame::Request {
            id: id.into(),
            method: method.to_string(),
            params,
        }
    }

    pub fn notification(method: &str, params: Value) -> Frame {
        Frame::Notification {
            method: method.to_string(),
            params,
        }
    }

    pub fn response(id: impl Into<Value>, result: Value) -> Frame {
        Frame::Response {
            id: id.into(),
            result,
        }
    }
}

/// `initialize` params. Only the fields Destro honours are modelled; unknown
/// fields are accepted and ignored, as the protocol requires.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub approval_requests: bool,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    #[serde(default)]
    pub user_questions: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub server_info: ServerInfo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

impl Default for InitializeResult {
    fn default() -> Self {
        InitializeResult {
            server_info: ServerInfo {
                name: "destro-harness".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        }
    }
}

/// One prompt content block. Images are admitted on the wire but only text is
/// executed by the phase-0 runtime.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            ContentBlock::Image { .. } => None,
        }
    }
}

/// Flatten content blocks to plain text, the way the runtime renders them for
/// models that were sent a block array.
pub fn blocks_text(blocks: &[ContentBlock]) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(blocks.len());
    for b in blocks {
        match b {
            ContentBlock::Text { text } => out.push(text),
            ContentBlock::Image { .. } => out.push("[image]"),
        }
    }
    out.join("\n")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionQueryParams {
    pub session_id: String,
    #[serde(default)]
    pub content_blocks: Vec<ContentBlock>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionQueryResult {
    pub kind: String,
    pub message_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEventNotification {
    pub session_id: String,
    pub event: Value,
}

impl SessionEventNotification {
    pub fn new(session_id: impl Into<String>, event: &SessionEvent) -> Self {
        SessionEventNotification {
            session_id: session_id.into(),
            event: event.to_value(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusNotification {
    pub session_id: String,
    /// `"running"` or `"idle"`.
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentStartedNotification {
    pub parent_session_id: String,
    pub child_session_id: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentFinishedNotification {
    pub child_session_id: String,
    /// `"ok"` or `"failed"`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestParams {
    pub session_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestResult {
    /// `allowed-once`, `rejected`, or `cancelled`.
    pub outcome: String,
}

/// `agent/spawn` params.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnParams {
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system: Option<String>,
}

/// The inspector view of one agent. Cheap to compute and safe to serialise.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    pub agent_id: String,
    pub session_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub model: String,
    /// `idle`, `running`, `failed`, or `killed`.
    pub status: String,
    pub turn: u32,
    pub queued_messages: usize,
    pub children: usize,
    pub transcript_len: usize,
    pub last_tool: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub created_ms_ago: u64,
    #[serde(default)]
    pub last_message: Option<String>,
}

/// How a message reaches a running agent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Delivered at the next turn boundary; never preempts the current request.
    #[default]
    Queue,
    /// Cancels the in-flight model request or tool, splices the message in, and
    /// continues the same turn. This is the steering verb.
    Inject,
}

impl Delivery {
    pub fn from_wire(s: Option<&str>) -> Delivery {
        match s {
            Some("inject") => Delivery::Inject,
            _ => Delivery::Queue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_frame_kind() {
        let req = Frame::parse(
            r#"{"jsonrpc":"2.0","id":7,"method":"session/query","params":{"sessionId":"s"}}"#,
        )
        .unwrap();
        assert!(matches!(req, Frame::Request { ref method, .. } if method == "session/query"));

        let note =
            Frame::parse(r#"{"jsonrpc":"2.0","method":"session.event","params":{}}"#).unwrap();
        assert!(
            matches!(note, Frame::Notification { ref method, .. } if method == "session.event")
        );

        let res = Frame::parse(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#).unwrap();
        assert!(matches!(res, Frame::Response { .. }));

        let err =
            Frame::parse(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}}"#)
                .unwrap();
        assert!(matches!(err, Frame::Error { code: -32601, .. }));
    }

    #[test]
    fn ignores_malformed_lines() {
        assert!(Frame::parse("").is_none());
        assert!(Frame::parse("not json").is_none());
        assert!(Frame::parse(r#"{"jsonrpc":"2.0"}"#).is_none());
    }

    #[test]
    fn frame_round_trips_through_json() {
        let f = Frame::request(
            3,
            ext::AGENT_SEND,
            json!({"agentId":"a1","text":"go","mode":"inject"}),
        );
        let back = Frame::parse(&f.to_line()).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn delivery_defaults_to_queue() {
        assert_eq!(Delivery::from_wire(None), Delivery::Queue);
        assert_eq!(Delivery::from_wire(Some("inject")), Delivery::Inject);
        assert_eq!(Delivery::from_wire(Some("other")), Delivery::Queue);
    }

    #[test]
    fn blocks_flatten_to_text() {
        let blocks = vec![
            ContentBlock::text("hello"),
            ContentBlock::Image {
                data: "x".into(),
                mime_type: "image/png".into(),
            },
            ContentBlock::text("world"),
        ];
        assert_eq!(blocks_text(&blocks), "hello\n[image]\nworld");
    }
}
