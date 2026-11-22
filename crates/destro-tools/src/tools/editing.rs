//! The `edit` tool: exact string replacement inside a UTF-8 file.

use crate::args;
use crate::paths;
use crate::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

/// Replaces exact occurrences of one string with another.
#[derive(Clone, Copy, Debug, Default)]
pub struct EditTool;

impl EditTool {
    /// Creates the tool.
    pub fn new() -> Self {
        Self
    }
}

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Replace an exact string in a text file, optionally replacing all occurrences."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to edit; relative paths resolve against the working directory."
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of exactly one (default false)."
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let raw = match args::require_str(inv, "path") {
                Ok(raw) => raw,
                Err(output) => return output,
            };
            let old_string = match args::require_str(inv, "old_string") {
                Ok(value) => value,
                Err(output) => return output,
            };
            let new_string = match args::require_str(inv, "new_string") {
                Ok(value) => value,
                Err(output) => return output,
            };
            if old_string.is_empty() {
                return ToolOutput::err("argument \"old_string\" must not be empty");
            }
            let replace_all = args::opt_bool(inv, "replace_all").unwrap_or(false);
            let path = paths::resolve(cx, raw);
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(content) => content,
                Err(error) => {
                    return ToolOutput::err(format!("cannot read \"{}\": {error}", path.display()));
                }
            };

            let occurrences = content.matches(old_string).count();
            if occurrences == 0 {
                return ToolOutput::err("old_string not found");
            }
            if occurrences > 1 && !replace_all {
                return ToolOutput::err(format!(
                    "old_string found {occurrences} times; pass replace_all=true to replace every occurrence"
                ));
            }
            let updated = if replace_all {
                content.replace(old_string, new_string)
            } else {
                content.replacen(old_string, new_string, 1)
            };
            let replaced = if replace_all { occurrences } else { 1 };
            match tokio::fs::write(&path, updated.as_bytes()).await {
                Ok(()) => ToolOutput::ok(format!(
                    "replaced {replaced} occurrence(s) in {}",
                    path.display()
                )),
                Err(error) => {
                    ToolOutput::err(format!("cannot write \"{}\": {error}", path.display()))
                }
            }
        })
    }
}
