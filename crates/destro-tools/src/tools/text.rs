//! The `echo` tool: deterministic JSON pretty-printer used by tests.

use crate::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

/// Returns the whole argument object as pretty-printed JSON.
#[derive(Clone, Copy, Debug, Default)]
pub struct EchoTool;

impl EchoTool {
    /// Creates the tool.
    pub fn new() -> Self {
        Self
    }
}

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "Return the argument object as pretty-printed JSON."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "description": "Any JSON object; it is echoed back verbatim.",
            "additionalProperties": true
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match serde_json::to_string_pretty(&inv.arguments) {
                Ok(text) => ToolOutput::ok(text),
                Err(error) => ToolOutput::err(format!("cannot serialize arguments: {error}")),
            }
        })
    }
}
