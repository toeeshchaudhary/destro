//! Model-facing tools for the destro agent harness.
//!
//! The crate is pure, synchronous-friendly async code: every tool is a plain
//! value that implements [`Tool`], and the only process a tool starts is
//! `pwsh`. There is no global mutable state — a [`Registry`] owns its tools.
//!
//! Argument handling is deliberately tolerant: malformed or missing
//! arguments produce [`ToolOutput::err`] instead of a panic.

#![deny(missing_docs)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

mod args;
mod paths;
pub mod tools;

/// A boxed, sendable future returned by [`Tool::run`].
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// One call of a tool, including the caller-provided identifier.
#[derive(Clone, Debug)]
pub struct ToolInvocation {
    /// Correlation id chosen by the caller and echoed back in tool results.
    pub call_id: String,
    /// Raw JSON arguments; tools extract what they need.
    pub arguments: serde_json::Value,
}

/// The result of running a tool.
#[derive(Clone, Debug, Default)]
pub struct ToolOutput {
    /// Human- and model-readable output text.
    pub content: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

impl ToolOutput {
    /// Creates a successful output.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// Creates a failed output.
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Per-call execution context.
pub struct ToolContext {
    /// Directory used to resolve relative paths.
    pub cwd: PathBuf,
    /// Wire id of the agent that invoked the tool (e.g. `"agent-000123"`).
    pub agent_id: String,
}

/// A named tool with a JSON schema and an async entry point.
///
/// Implementations must be cheap to clone by `Arc`, hence `Send + Sync`.
pub trait Tool: Send + Sync {
    /// The unique tool name.
    fn name(&self) -> &'static str;
    /// A short description shown to the model.
    fn description(&self) -> &'static str;
    /// JSON Schema object describing the accepted arguments.
    fn schema(&self) -> serde_json::Value;
    /// Runs the tool. Malformed arguments must be reported as an error output.
    fn run<'a>(&'a self, inv: &'a ToolInvocation, cx: &'a ToolContext)
    -> BoxFuture<'a, ToolOutput>;
}

/// A set of tools addressable by name.
#[derive(Default)]
pub struct Registry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Registry {
    /// Creates a registry containing every built-in tool.
    ///
    /// The built-ins are `read`, `write`, `edit`, `glob`, `grep`, `pwsh` and
    /// `echo`.
    pub fn builtin() -> Registry {
        let mut registry = Registry::default();
        registry.register(Arc::new(tools::ReadTool));
        registry.register(Arc::new(tools::WriteTool));
        registry.register(Arc::new(tools::EditTool));
        registry.register(Arc::new(tools::GlobTool));
        registry.register(Arc::new(tools::GrepTool));
        registry.register(Arc::new(tools::PwshTool));
        registry.register(Arc::new(tools::EchoTool));
        registry
    }

    /// Creates a registry from an explicit list of tools.
    pub fn from_tools(tools: Vec<Arc<dyn Tool>>) -> Registry {
        let mut registry = Registry::default();
        for tool in tools {
            registry.register(tool);
        }
        registry
    }

    /// Adds a tool, replacing any existing tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Looks a tool up by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Returns all registered tool names in sorted order.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.tools.values().map(|tool| tool.name()).collect();
        names.sort_unstable();
        names
    }

    /// Returns schemas for all registered tools, sorted by name.
    ///
    /// Each entry has the shape `{ "name", "description", "parameters" }`.
    pub fn schemas(&self) -> Vec<serde_json::Value> {
        let mut names = self.names();
        names.sort_unstable();
        names
            .into_iter()
            .filter_map(|name| {
                let tool = self.tools.get(name)?;
                Some(serde_json::json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "parameters": tool.schema(),
                }))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_is_sorted_and_complete() {
        let registry = Registry::builtin();
        assert_eq!(
            registry.names(),
            vec!["echo", "edit", "glob", "grep", "pwsh", "read", "write"]
        );
        let schemas = registry.schemas();
        assert_eq!(schemas.len(), 7);
        assert_eq!(schemas[0]["name"], "echo");
        assert!(
            schemas
                .iter()
                .all(|schema| schema["parameters"].is_object())
        );
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn register_replaces_and_default_is_empty() {
        let mut registry = Registry::default();
        assert!(registry.names().is_empty());
        registry.register(Arc::new(tools::EchoTool::new()));
        registry.register(Arc::new(tools::EchoTool));
        assert_eq!(registry.names(), vec!["echo"]);
        let registry = Registry::from_tools(vec![Arc::new(tools::EchoTool::new())]);
        assert!(registry.get("echo").is_some());
    }
}
