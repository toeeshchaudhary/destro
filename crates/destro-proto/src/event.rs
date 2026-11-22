//! The session event vocabulary.
//!
//! A session log is an ordered sequence of these events; each one is also what a
//! `session.event` notification carries. The `type` and `data` names are the dsh
//! SDK's, so existing transcript readers (including the TUI) fold them unchanged.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Token accounting from one model response.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Uncached plus cached input on the latest request: the live context size.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn accumulate(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }
}

/// One committed session event.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    TurnStart {
        turn: u32,
    },
    TurnEnd {
        turn: u32,
        kind: TurnEndKind,
    },
    UserMessage {
        text: String,
        source: MessageSource,
    },
    AssistantMessage {
        text: String,
        reasoning: Option<String>,
        usage: Option<Usage>,
        interrupted: bool,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        name: String,
        is_error: bool,
        content: String,
    },
    Notice {
        text: String,
    },
    SessionTitle {
        title: String,
    },
    ContextWindow {
        window: u64,
    },
    /// Older messages are being folded into a summary.
    CompactionStart,
    /// The fold finished: `summary` replaces everything before the last
    /// `keep` messages.
    CompactionEnd {
        summary: String,
        keep: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnEndKind {
    /// The model answered with no tool calls.
    Done,
    /// The operator or a parent interrupted the turn.
    Interrupted,
    /// The provider or a tool failed.
    Failed,
}

impl TurnEndKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnEndKind::Done => "done",
            TurnEndKind::Interrupted => "interrupted",
            TurnEndKind::Failed => "failed",
        }
    }
}

/// Who produced a user-role message. Synthetic injections (file notices, cron,
/// skills, subagent prompts) are user-role but not human prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageSource {
    User,
    Subagent,
    System,
}

impl MessageSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageSource::User => "user",
            MessageSource::Subagent => "subagent",
            MessageSource::System => "system",
        }
    }
}

impl SessionEvent {
    /// The `{ "type": ..., "data": ... }` envelope a `session.event` carries.
    pub fn to_value(&self) -> Value {
        match self {
            SessionEvent::TurnStart { turn } => json!({
                "type": "turn/start",
                "data": { "turn": turn },
            }),
            SessionEvent::TurnEnd { turn, kind } => json!({
                "type": "turn/end",
                "data": { "turn": turn, "reason": { "kind": kind.as_str() } },
            }),
            SessionEvent::UserMessage { text, source } => json!({
                "type": "user/message",
                "data": {
                    "content": [{ "type": "text", "text": text }],
                    "source": { "kind": source.as_str() },
                },
            }),
            SessionEvent::AssistantMessage {
                text,
                reasoning,
                usage,
                interrupted,
            } => {
                let mut content: Vec<Value> = Vec::with_capacity(2);
                if let Some(r) = reasoning.as_ref().filter(|r| !r.is_empty()) {
                    content.push(json!({ "type": "reasoning", "text": r }));
                }
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                let mut data = json!({
                    "message": { "role": "assistant", "content": content },
                    "interrupted": interrupted,
                });
                if let Some(u) = usage {
                    data["usage"] = json!({
                        "inputTokens": u.input_tokens,
                        "outputTokens": u.output_tokens,
                        "cacheReadTokens": u.cache_read_tokens,
                        "cacheWriteTokens": u.cache_write_tokens,
                        "reasoningTokens": u.reasoning_tokens,
                    });
                }
                json!({ "type": "assistant/message", "data": data })
            }
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => json!({
                "type": "tool/call",
                "data": { "callId": call_id, "name": name, "arguments": arguments },
            }),
            SessionEvent::ToolResult {
                call_id,
                name,
                is_error,
                content,
            } => json!({
                "type": "tool/result",
                "data": {
                    "message": {
                        "role": "tool",
                        "toolCallId": call_id,
                        "name": name,
                        "isError": is_error,
                        "content": [{ "type": "text", "text": content }],
                    }
                },
            }),
            SessionEvent::Notice { text } => json!({
                "type": "notice",
                "data": { "text": text },
            }),
            SessionEvent::SessionTitle { title } => json!({
                "type": "session/title",
                "data": { "title": title },
            }),
            SessionEvent::ContextWindow { window } => json!({
                "type": "request/context",
                "data": { "contextWindow": window },
            }),
            SessionEvent::CompactionStart => json!({
                "type": "compaction/start",
                "data": {},
            }),
            SessionEvent::CompactionEnd { summary, keep } => json!({
                "type": "compaction/end",
                "data": { "summary": summary, "keep": keep },
            }),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            SessionEvent::TurnStart { .. } => "turn/start",
            SessionEvent::TurnEnd { .. } => "turn/end",
            SessionEvent::UserMessage { .. } => "user/message",
            SessionEvent::AssistantMessage { .. } => "assistant/message",
            SessionEvent::ToolCall { .. } => "tool/call",
            SessionEvent::ToolResult { .. } => "tool/result",
            SessionEvent::Notice { .. } => "notice",
            SessionEvent::SessionTitle { .. } => "session/title",
            SessionEvent::ContextWindow { .. } => "request/context",
            SessionEvent::CompactionStart => "compaction/start",
            SessionEvent::CompactionEnd { .. } => "compaction/end",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_the_dsh_envelope() {
        let v = SessionEvent::UserMessage {
            text: "hi".into(),
            source: MessageSource::User,
        }
        .to_value();
        assert_eq!(v["type"], "user/message");
        assert_eq!(v["data"]["content"][0]["text"], "hi");
        assert_eq!(v["data"]["source"]["kind"], "user");
    }

    #[test]
    fn assistant_usage_uses_camel_case() {
        let v = SessionEvent::AssistantMessage {
            text: "ok".into(),
            reasoning: None,
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_tokens: 5,
                ..Usage::default()
            }),
            interrupted: false,
        }
        .to_value();
        assert_eq!(v["data"]["usage"]["inputTokens"], 10);
        assert_eq!(v["data"]["usage"]["cacheReadTokens"], 5);
        assert_eq!(v["data"]["interrupted"], false);
    }

    #[test]
    fn context_tokens_counts_cached_input() {
        let u = Usage {
            input_tokens: 3,
            cache_read_tokens: 7,
            cache_write_tokens: 2,
            ..Usage::default()
        };
        assert_eq!(u.context_tokens(), 12);
    }
}
