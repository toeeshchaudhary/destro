//! Streaming client for the OpenAI chat-completions dialect.
//!
//! This covers any vendor that speaks the OpenAI `/chat/completions` streaming
//! protocol verbatim: OpenAI, DeepSeek, vLLM, LM Studio, Ollama's compatibility
//! endpoint, and so on. The HTTP request is sent from a Tokio task and items are
//! relayed through a bounded channel, so the returned stream is `'static` and
//! cancellation stops the request instead of merely detaching it.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::mpsc::{Sender, channel};
use tokio_util::sync::CancellationToken;

use destro_proto::event::Usage;

use super::sse::{SseEvent, SseParser};
use super::{ChatRequest, Provider, ProviderError, ProviderItem, ProviderStream};

/// How many items may sit between the HTTP reader and the agent loop.
const CHANNEL_CAPACITY: usize = 64;

/// A provider speaking the OpenAI chat-completions streaming API.
///
/// Request construction is pure ([`build_body`]); parsing is pure
/// ([`StreamState`] plus [`SseParser`]); only [`Provider::stream`] does I/O.
pub struct OpenAiCompatProvider {
    name: &'static str,
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    /// Build a provider for `base_url` (for example
    /// `https://api.deepseek.com/v1`) authenticating with `api_key`.
    ///
    /// The endpoint used is `{base_url}/chat/completions`; a trailing slash on
    /// `base_url` is ignored. An empty `api_key` sends no `Authorization`
    /// header, which suits local servers that need none.
    pub fn new(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            name,
            base_url: base_url.into(),
            api_key: api_key.into(),
            client,
        }
    }

    /// The configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let job = HttpJob {
            client: self.client.clone(),
            url: format!("{}/chat/completions", self.base_url.trim_end_matches('/')),
            api_key: self.api_key.clone(),
            body: build_body(&req),
            label: self.name,
        };
        let (tx, rx) = channel(CHANNEL_CAPACITY);
        tokio::spawn(async move {
            if let Err(err) = job.run(cancel, &tx).await {
                let _ = tx.send(Err(err)).await;
            }
        });
        stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed()
    }
}

/// Build the JSON body of one `chat/completions` request.
///
/// `tools` arrive in the neutral `{name, description, parameters}` shape and
/// are wrapped in OpenAI's `{"type":"function","function":{…}}` envelope.
pub(crate) fn build_body(req: &ChatRequest) -> Value {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        messages.push(json!({ "role": "system", "content": req.system }));
    }
    messages.extend(req.messages.iter().cloned());

    let mut body = json!({
        "model": req.model,
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(openai_tool).collect());
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    body
}

fn openai_tool(tool: &Value) -> Value {
    // Already in OpenAI form (a caller may pass vendor-native schemas).
    if tool.get("type").and_then(Value::as_str) == Some("function") {
        return tool.clone();
    }
    json!({
        "type": "function",
        "function": {
            "name": tool.get("name").cloned().unwrap_or(Value::Null),
            "description": tool.get("description").cloned().unwrap_or_else(|| json!("")),
            "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
        }
    })
}

/// One HTTP request, moved into the worker task.
struct HttpJob {
    client: reqwest::Client,
    url: String,
    api_key: String,
    body: Value,
    label: &'static str,
}

impl HttpJob {
    /// Run the request, forwarding items to `tx`.
    ///
    /// Returns `Ok(())` when the stream ended or the consumer went away, and
    /// [`ProviderError`] when the exchange failed or was cancelled.
    async fn run(
        &self,
        cancel: CancellationToken,
        tx: &Sender<Result<ProviderItem, ProviderError>>,
    ) -> Result<(), ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let mut request = self.client.post(&self.url).json(&self.body);
        if !self.api_key.is_empty() {
            request = request.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key),
            );
        }

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = request.send() => result.map_err(|e| {
                self.failure(format!("request to {} failed: {e}", self.url))
            })?,
        };

        let status = response.status();
        if !status.is_success() {
            let detail = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                text = response.text() => text.unwrap_or_default(),
            };
            return Err(self.failure(format!(
                "HTTP {} from {}: {}",
                status.as_u16(),
                self.url,
                shorten(&detail)
            )));
        }

        let mut parser = SseParser::new();
        let mut state = StreamState::default();
        let mut bytes = response.bytes_stream();

        'read: loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                chunk = bytes.next() => chunk,
            };
            match chunk {
                None => break,
                Some(Err(e)) => {
                    return Err(self.failure(format!("stream from {} failed: {e}", self.url)));
                }
                Some(Ok(chunk)) => {
                    for event in parser.push(&chunk) {
                        match event {
                            SseEvent::Done => break 'read,
                            SseEvent::Data(data) => {
                                if !forward(state.handle_data(&data), tx, &cancel).await {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        }

        // The transport may end without a trailing blank line.
        for event in parser.finish() {
            if let SseEvent::Data(data) = event {
                if !forward(state.handle_data(&data), tx, &cancel).await {
                    return Ok(());
                }
            }
        }

        forward(state.finish(), tx, &cancel).await;
        Ok(())
    }

    fn failure(&self, message: String) -> ProviderError {
        ProviderError::Failed(format!("{}: {message}", self.label))
    }
}

/// Send items, returning false when the consumer is gone or cancelling.
async fn forward(
    items: Vec<ProviderItem>,
    tx: &Sender<Result<ProviderItem, ProviderError>>,
    cancel: &CancellationToken,
) -> bool {
    for item in items {
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            result = tx.send(Ok(item)) => result.is_ok(),
        };
        if !sent {
            return false;
        }
    }
    true
}

/// Tool-call fragments accumulated for one `index`.
#[derive(Debug, Default)]
struct ToolCallParts {
    id: String,
    name: String,
    arguments: String,
}

/// SSE response state: text deltas pass straight through, tool calls and usage
/// are accumulated until the turn ends.
#[derive(Debug, Default)]
struct StreamState {
    tools: BTreeMap<u64, ToolCallParts>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl StreamState {
    /// Consume one `data:` payload, returning items worth emitting immediately.
    ///
    /// Malformed JSON is skipped rather than failing the stream.
    fn handle_data(&mut self, data: &str) -> Vec<ProviderItem> {
        let mut out = Vec::new();
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return out;
        };

        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(usage_from_json(usage));
        }

        let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) else {
            return out;
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.push(ProviderItem::TextDelta(text.to_string()));
                }
            }
            let reasoning = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str);
            if let Some(text) = reasoning {
                if !text.is_empty() {
                    out.push(ProviderItem::ReasoningDelta(text.to_string()));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, call) in calls.iter().enumerate() {
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(position as u64);
                    self.absorb_tool_call(index, call);
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        out
    }

    fn absorb_tool_call(&mut self, index: u64, call: &Value) {
        let parts = self.tools.entry(index).or_default();
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                parts.id = id.to_string();
            }
        }
        if let Some(function) = call.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                if !name.is_empty() {
                    parts.name = name.to_string();
                }
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                parts.arguments.push_str(arguments);
            }
        }
    }

    /// Items that only make sense once the response is complete: usage, one
    /// tool call per index in index order, then `Done`.
    fn finish(&mut self) -> Vec<ProviderItem> {
        let mut out = Vec::new();
        if let Some(usage) = self.usage.take() {
            out.push(ProviderItem::Usage(usage));
        }
        for (index, parts) in std::mem::take(&mut self.tools) {
            out.push(ProviderItem::ToolCall {
                id: if parts.id.is_empty() {
                    format!("call_{index}")
                } else {
                    parts.id
                },
                name: parts.name,
                arguments: parts.arguments,
            });
        }
        out.push(ProviderItem::Done {
            finish_reason: self
                .finish_reason
                .take()
                .unwrap_or_else(|| "stop".to_string()),
        });
        out
    }
}

/// Map OpenAI's `usage` object onto the neutral [`Usage`].
fn usage_from_json(value: &Value) -> Usage {
    let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    let nested = |parent: &str, child: &str| {
        value
            .get(parent)
            .and_then(|details| details.get(child))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| field("input_tokens")),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| field("output_tokens")),
        cache_read_tokens: nested("prompt_tokens_details", "cached_tokens"),
        cache_write_tokens: 0,
        reasoning_tokens: nested("completion_tokens_details", "reasoning_tokens"),
    }
}

/// One-line, length-capped rendering of an error body.
fn shorten(body: &str) -> String {
    let cleaned = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return "<empty body>".to_string();
    }
    const LIMIT: usize = 300;
    if cleaned.chars().count() <= LIMIT {
        return cleaned;
    }
    let mut truncated: String = cleaned.chars().take(LIMIT).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::user_message;

    fn data(payload: &str) -> Vec<ProviderItem> {
        StreamState::default().handle_data(payload)
    }

    #[test]
    fn body_carries_system_stream_options_tools_and_cap() {
        let req = ChatRequest {
            model: "deepseek-chat".into(),
            system: "be brief".into(),
            messages: vec![user_message("hi")],
            tools: vec![json!({
                "name": "read",
                "description": "read a file",
                "parameters": { "type": "object" }
            })],
            max_tokens: Some(64),
        };
        let body = build_body(&req);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be brief");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["tools"][0]["function"]["description"], "read a file");
        assert_eq!(body["max_tokens"], 64);
    }

    #[test]
    fn body_omits_empty_system_tools_and_cap() {
        let req = ChatRequest {
            messages: vec![user_message("hi")],
            ..ChatRequest::default()
        };
        let body = build_body(&req);
        assert!(body.get("tools").is_none());
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn text_and_reasoning_deltas_are_forwarded() {
        let items = data(
            r#"{"choices":[{"delta":{"reasoning_content":"think ","content":"hello"},"finish_reason":null}]}"#,
        );
        assert_eq!(
            items,
            vec![
                ProviderItem::TextDelta("hello".into()),
                ProviderItem::ReasoningDelta("think ".into()),
            ]
        );
    }

    #[test]
    fn reasoning_alias_is_accepted() {
        let items = data(r#"{"choices":[{"delta":{"reasoning":"hm"}}]}"#);
        assert_eq!(items, vec![ProviderItem::ReasoningDelta("hm".into())]);
    }

    #[test]
    fn finish_reason_is_recorded_and_defaults_to_stop() {
        let mut state = StreamState::default();
        state.handle_data(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
        let items = state.finish();
        assert!(matches!(
            items.last(),
            Some(ProviderItem::Done { finish_reason }) if finish_reason == "tool_calls"
        ));

        let items = StreamState::default().finish();
        assert!(matches!(
            items.as_slice(),
            [ProviderItem::Done { finish_reason }] if finish_reason == "stop"
        ));
    }

    #[test]
    fn fragmented_tool_arguments_are_accumulated_by_index() {
        let mut state = StreamState::default();
        state.handle_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
        );
        state.handle_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        );
        let items = state.finish();
        assert_eq!(
            items,
            vec![
                ProviderItem::ToolCall {
                    id: "call_a".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.txt\"}".into(),
                },
                ProviderItem::Done {
                    finish_reason: "stop".into(),
                },
            ]
        );
    }

    #[test]
    fn parallel_tool_calls_are_emitted_in_index_order_before_done() {
        let mut state = StreamState::default();
        state.handle_data(
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":1,"id":"call_b","function":{"name":"write","arguments":"{}"}},
                {"index":0,"id":"call_a","function":{"name":"read","arguments":"{}"}}
            ]}}]}"#,
        );
        let items = state.finish();
        let calls: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                ProviderItem::ToolCall { id, name, .. } => Some((id.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec![("call_a", "read"), ("call_b", "write")]);
        assert!(matches!(items.last(), Some(ProviderItem::Done { .. })));
    }

    #[test]
    fn usage_is_mapped_and_emitted_before_done() {
        let mut state = StreamState::default();
        // Usage arrives in a final chunk whose `choices` array is empty.
        state.handle_data(
            r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,
                "prompt_tokens_details":{"cached_tokens":3},
                "completion_tokens_details":{"reasoning_tokens":2}}}"#,
        );
        let items = state.finish();
        assert_eq!(
            items[0],
            ProviderItem::Usage(Usage {
                input_tokens: 11,
                output_tokens: 7,
                cache_read_tokens: 3,
                cache_write_tokens: 0,
                reasoning_tokens: 2,
            })
        );
        assert!(matches!(items.last(), Some(ProviderItem::Done { .. })));
    }

    #[test]
    fn malformed_json_is_skipped() {
        assert!(data("not json at all").is_empty());
        assert!(data("").is_empty());
        let mut state = StreamState::default();
        state.handle_data("{broken");
        assert_eq!(state.finish().len(), 1);
    }

    #[tokio::test]
    async fn an_already_cancelled_request_yields_cancelled() {
        let provider = OpenAiCompatProvider::new("test", "http://127.0.0.1:9/v1", "unused-key");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut stream = provider.stream(ChatRequest::default(), cancel);
        let first = stream.next().await.expect("one item");
        assert!(matches!(first, Err(ProviderError::Cancelled)));
    }
}
