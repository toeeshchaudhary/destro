//! Deterministic, network-free provider used by tests and demos.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{ChatRequest, Provider, ProviderError, ProviderItem, ProviderStream, last_user_text};
use destro_proto::event::Usage;

/// A scripted reply for a [`MockProvider`].
#[derive(Clone, Debug, Default)]
pub struct MockTurn {
    /// Assistant text to stream in chunks.
    pub text: String,
    /// Optional reasoning text streamed before `text`.
    pub reasoning: Option<String>,
    /// Tool calls emitted after the text.
    pub tool_calls: Vec<MockToolCall>,
    /// Token accounting reported to the loop.
    pub usage: Usage,
    /// Finish reason; empty means `"stop"`.
    pub finish_reason: String,
    /// When set, the stream yields this error after any text instead of `Done`.
    pub error: Option<String>,
}

impl MockTurn {
    /// A plain text reply that finishes with `"stop"`.
    pub fn text(text: impl Into<String>) -> Self {
        MockTurn {
            text: text.into(),
            finish_reason: "stop".into(),
            ..MockTurn::default()
        }
    }

    /// Append one tool call to the reply.
    pub fn with_tool_call(mut self, call: MockToolCall) -> Self {
        self.tool_calls.push(call);
        self
    }
}

/// One scripted tool call.
#[derive(Clone, Debug)]
pub struct MockToolCall {
    /// Provider-issued call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool argument object.
    pub arguments: Value,
}

impl MockToolCall {
    /// Build a tool call from its id, name, and arguments.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        MockToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

type Responder = Arc<dyn Fn(&ChatRequest) -> MockTurn + Send + Sync>;

/// A deterministic provider. Cheap enough to run a thousand agents in one test.
pub struct MockProvider {
    respond: Responder,
    chunk_chars: usize,
    delay: Option<Duration>,
}

impl MockProvider {
    /// Build from a rule: the rule sees each request and returns its reply.
    pub fn new(respond: impl Fn(&ChatRequest) -> MockTurn + Send + Sync + 'static) -> Self {
        MockProvider {
            respond: Arc::new(respond),
            chunk_chars: 24,
            delay: None,
        }
    }

    /// Echo the last user message back. Useful for demos and smoke tests.
    pub fn echo() -> Self {
        MockProvider::new(|req| MockTurn::text(last_user_text(req)))
    }

    /// Always answer with the same text.
    pub fn constant(text: impl Into<String>) -> Self {
        let text = Arc::new(text.into());
        let reply = text.clone();
        MockProvider::new(move |_| MockTurn::text(reply.as_str()))
    }

    /// No text, no tool calls: a one-pass turn that ends immediately.
    pub fn silent() -> Self {
        MockProvider::new(|_| MockTurn::default())
    }

    /// Split streamed text into chunks of this many characters.
    pub fn with_chunk_chars(mut self, n: usize) -> Self {
        self.chunk_chars = n.max(1);
        self
    }

    /// Add a delay before each streamed chunk, to make concurrency observable.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let turn = (self.respond)(&req);
        let mut items: Vec<Result<ProviderItem, ProviderError>> = Vec::new();
        if let Some(r) = turn.reasoning {
            for chunk in chunks(&r, self.chunk_chars) {
                items.push(Ok(ProviderItem::ReasoningDelta(chunk)));
            }
        }
        for chunk in chunks(&turn.text, self.chunk_chars) {
            items.push(Ok(ProviderItem::TextDelta(chunk)));
        }
        for call in turn.tool_calls {
            items.push(Ok(ProviderItem::ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments.to_string(),
            }));
        }
        if turn.usage != Usage::default() {
            items.push(Ok(ProviderItem::Usage(turn.usage)));
        }
        match turn.error {
            Some(err) => items.push(Err(ProviderError::Failed(err))),
            None => items.push(Ok(ProviderItem::Done {
                finish_reason: if turn.finish_reason.is_empty() {
                    "stop".into()
                } else {
                    turn.finish_reason
                },
            })),
        }

        match self.delay {
            None => stream::iter(items).boxed(),
            Some(delay) => {
                let stream = stream::iter(items)
                    .then(move |item| {
                        let cancel = cancel.clone();
                        async move {
                            if cancel.is_cancelled() {
                                return None;
                            }
                            tokio::time::sleep(delay).await;
                            Some(item)
                        }
                    })
                    .take_while(|x| std::future::ready(x.is_some()))
                    .filter_map(|x| async move { x });
                stream.boxed()
            }
        }
    }
}

fn chunks(text: &str, size: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if cur.chars().count() >= size {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::user_message;
    use futures_util::StreamExt;
    use serde_json::json;

    async fn collect(p: &MockProvider, text: &str) -> Vec<ProviderItem> {
        let req = ChatRequest {
            messages: vec![user_message(text)],
            ..ChatRequest::default()
        };
        p.stream(req, CancellationToken::new())
            .filter_map(|i| async move { i.ok() })
            .collect()
            .await
    }

    #[tokio::test]
    async fn echo_streams_the_user_text_back_in_chunks() {
        let items = collect(&MockProvider::echo().with_chunk_chars(2), "abcdef").await;
        let text: String = items
            .iter()
            .filter_map(|i| match i {
                ProviderItem::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "abcdef");
        assert!(matches!(items.last(), Some(ProviderItem::Done { .. })));
    }

    #[tokio::test]
    async fn tool_calls_come_after_text() {
        let p = MockProvider::new(|_| {
            MockTurn::text("looking").with_tool_call(MockToolCall::new(
                "c1",
                "echo",
                json!({"a": 1}),
            ))
        });
        let items = collect(&p, "go").await;
        let call = items
            .iter()
            .find_map(|i| match i {
                ProviderItem::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .expect("tool call emitted");
        assert_eq!(call.0, "c1");
        assert_eq!(call.1, "echo");
        assert!(call.2.contains("\"a\""));
    }

    #[tokio::test]
    async fn cancel_stops_the_delayed_stream() {
        let p = MockProvider::constant("abcdefghij").with_delay(Duration::from_millis(5));
        let cancel = CancellationToken::new();
        let req = ChatRequest::default();
        let mut s = p.stream(req, cancel.clone());
        // Consume one item, then cancel: the stream must end soon.
        assert!(s.next().await.is_some());
        cancel.cancel();
        let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(1), s.collect::<Vec<_>>())
            .await
            .expect("stream ended");
        assert!(rest.len() < 10);
    }
}
