//! Model providers.
//!
//! The agent loop only sees this trait: a request goes in, a stream of deltas and
//! complete tool calls comes out. Transport details (HTTP/2 pooling, SSE parsing,
//! retries) belong to the implementor, so the loop stays testable and the mock
//! provider can drive end-to-end runs with no key.
//!
//! Two implementations ship with the crate:
//!
//! - [`MockProvider`] — deterministic scripted replies, no network.
//! - [`OpenAiCompatProvider`] — a real streaming client for the OpenAI
//!   chat-completions API, which most vendors (DeepSeek, OpenAI, vLLM, Ollama,
//!   …) speak.
//!
//! [`ProviderRegistry`] describes the providers a user may pick from (built-in
//! entries plus an optional JSON config file) and builds them on demand, so a
//! new backend does not need changes in the agent loop.

use futures_core::stream::BoxStream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use destro_proto::event::Usage;

mod mock;
mod openai_compat;
mod registry;
mod sse;

pub use mock::{MockProvider, MockToolCall, MockTurn};
pub use openai_compat::OpenAiCompatProvider;
pub use registry::{ProviderDescriptor, ProviderInfo, ProviderKind, ProviderRegistry};
pub use sse::{SseEvent, SseParser};

/// One model request. Messages use the neutral chat shape
/// (`{role, content}` plus optional `tool_calls` / `tool_call_id`) so a provider
/// adapter owns its own dialect.
#[derive(Clone, Debug, Default)]
pub struct ChatRequest {
    /// Model id the harness selected for this request.
    pub model: String,
    /// System prompt; empty means "no system message".
    pub system: String,
    /// Conversation so far in the neutral chat shape.
    pub messages: Vec<Value>,
    /// Tool schemas: `[{name, description, parameters}]`.
    pub tools: Vec<Value>,
    /// Per-response output cap, when the caller sets one.
    pub max_tokens: Option<u64>,
}

/// One item of a provider stream.
#[derive(Clone, Debug, PartialEq)]
pub enum ProviderItem {
    /// A fragment of assistant text.
    TextDelta(String),
    /// A fragment of reasoning/thinking text, when the model exposes it.
    ReasoningDelta(String),
    /// A complete tool call. Providers that stream partial arguments must
    /// accumulate before emitting this.
    ToolCall {
        /// Provider-issued call id, echoed back with the tool result.
        id: String,
        /// Tool name.
        name: String,
        /// Raw JSON argument object encoded as a string.
        arguments: String,
    },
    /// Token accounting for the response, when the provider reports it.
    Usage(Usage),
    /// End of one response.
    Done {
        /// Provider finish reason, e.g. `"stop"` or `"tool_calls"`.
        finish_reason: String,
    },
}

/// A boxed, `'static` stream of provider items.
pub type ProviderStream = BoxStream<'static, Result<ProviderItem, ProviderError>>;

/// A provider failure.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ProviderError {
    /// The request was cancelled or the stream was dropped.
    #[error("request cancelled")]
    Cancelled,
    /// Transport or protocol failure, with a human-readable message.
    #[error("{0}")]
    Failed(String),
}

/// A model backend.
pub trait Provider: Send + Sync + 'static {
    /// Stable identifier used in diagnostics, e.g. `"mock"`.
    fn name(&self) -> &'static str;

    /// Start one streaming request. The returned stream must stop promptly when
    /// `cancel` fires; dropping it must not leak work.
    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream;
}

/// The last user-role message's text, or an empty string.
pub fn last_user_text(req: &ChatRequest) -> String {
    for msg in req.messages.iter().rev() {
        if msg["role"] == "user" {
            return match &msg["content"] {
                Value::String(s) => s.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
        }
    }
    String::new()
}

/// Build a user-role message in the neutral chat shape.
pub fn user_message(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_has_the_neutral_shape() {
        let msg = user_message("hi");
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"], "hi");
    }

    #[test]
    fn last_user_text_reads_the_newest_user_message() {
        let req = ChatRequest {
            messages: vec![
                user_message("first"),
                json!({"role": "assistant", "content": "reply"}),
                user_message("second"),
            ],
            ..ChatRequest::default()
        };
        assert_eq!(last_user_text(&req), "second");
    }

    #[test]
    fn last_user_text_joins_content_blocks() {
        let req = ChatRequest {
            messages: vec![json!({
                "role": "user",
                "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]
            })],
            ..ChatRequest::default()
        };
        assert_eq!(last_user_text(&req), "a\nb");
    }
}
