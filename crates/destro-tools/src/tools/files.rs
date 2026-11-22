//! The `read` and `write` file tools.

use crate::args;
use crate::paths;
use crate::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

/// Maximum number of bytes `read` emits.
const READ_OUTPUT_CAP: usize = 256 * 1024;
/// Default number of lines `read` emits.
const READ_DEFAULT_LIMIT: u64 = 2000;
/// Hard upper bound for the `limit` argument.
const READ_MAX_LIMIT: u64 = 4000;

/// Reads a UTF-8 text file with line numbers.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadTool;

impl ReadTool {
    /// Creates the tool.
    pub fn new() -> Self {
        Self
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file and return line-numbered contents."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to read; relative paths resolve against the working directory."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based first line to return (default 1).",
                    "minimum": 1
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default 2000, max 4000).",
                    "minimum": 1
                }
            },
            "required": ["path"],
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
            let path = paths::resolve(cx, raw);
            let metadata = match tokio::fs::metadata(&path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    return ToolOutput::err(format!("cannot read \"{}\": {error}", path.display()));
                }
            };
            if !metadata.is_file() {
                return ToolOutput::err(format!("not a file: {}", path.display()));
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return ToolOutput::err(format!("cannot read \"{}\": {error}", path.display()));
                }
            };
            let text = String::from_utf8_lossy(&bytes);

            let offset = args::opt_u64(inv, "offset").unwrap_or(1).max(1);
            let limit = args::opt_u64(inv, "limit")
                .unwrap_or(READ_DEFAULT_LIMIT)
                .clamp(1, READ_MAX_LIMIT);

            let lines: Vec<&str> = text.lines().collect();
            let total = lines.len();
            let start = ((offset - 1) as usize).min(total);
            let end = start.saturating_add(limit as usize).min(total);

            let mut body = String::new();
            for (index, line) in lines[start..end].iter().enumerate() {
                body.push_str(&format!("{:>6} {line}\n", start + index + 1));
            }
            let full_bytes = body.len();
            let mut content = body;
            if paths::truncate_bytes(&mut content, READ_OUTPUT_CAP) {
                content.push_str(&format!(
                    "\n[truncated: showing {} of {full_bytes} bytes]",
                    content.len()
                ));
            }
            if end < total {
                content.push_str(&format!("\n[total lines: {total}]"));
            }
            ToolOutput::ok(content)
        })
    }
}

/// Writes UTF-8 content to a file, creating parent directories.
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteTool;

impl WriteTool {
    /// Creates the tool.
    pub fn new() -> Self {
        Self
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a file with UTF-8 content."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to write; relative paths resolve against the working directory."
                },
                "content": {
                    "type": "string",
                    "description": "Full UTF-8 content of the file."
                }
            },
            "required": ["path", "content"],
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
            let content = match args::require_str(inv, "content") {
                Ok(content) => content,
                Err(output) => return output,
            };
            let path = paths::resolve(cx, raw);
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && let Err(error) = tokio::fs::create_dir_all(parent).await
            {
                return ToolOutput::err(format!(
                    "cannot create directory \"{}\": {error}",
                    parent.display()
                ));
            }
            match tokio::fs::write(&path, content.as_bytes()).await {
                Ok(()) => ToolOutput::ok(format!(
                    "wrote {} bytes to {}",
                    content.len(),
                    path.display()
                )),
                Err(error) => {
                    ToolOutput::err(format!("cannot write \"{}\": {error}", path.display()))
                }
            }
        })
    }
}
