//! The `pwsh` tool: run a PowerShell command in the working directory.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::args;
use crate::paths;
use crate::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

/// Default command timeout.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Minimum command timeout.
const MIN_TIMEOUT_MS: u64 = 1_000;
/// Maximum command timeout.
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Bytes of stdout/stderr retained while reading.
const STREAM_KEEP_BYTES: usize = 128 * 1024;
/// Maximum combined output size, taken from the tail.
const OUTPUT_CAP: usize = 100 * 1024;

/// Runs a PowerShell (`pwsh`) command.
#[derive(Clone, Copy, Debug, Default)]
pub struct PwshTool;

impl PwshTool {
    /// Creates the tool.
    pub fn new() -> Self {
        Self
    }
}

impl Tool for PwshTool {
    fn name(&self) -> &'static str {
        "pwsh"
    }

    fn description(&self) -> &'static str {
        "Run a PowerShell command in the working directory and capture its output."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "PowerShell command line passed to `pwsh -Command`."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 120000, min 1000, max 600000).",
                    "minimum": 1000,
                    "maximum": 600000
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let command = match args::require_str(inv, "command") {
                Ok(command) => command,
                Err(output) => return output,
            };
            let timeout_ms = args::opt_u64(inv, "timeout_ms")
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);

            // Windows ships Windows PowerShell 5.1 as `powershell.exe`; PowerShell 7
            // installs `pwsh`. Prefer the modern one, fall back so the tool works on
            // a stock machine.
            let mut child = None;
            let mut last_error = None;
            for exe in ["pwsh", "powershell.exe"] {
                let mut process = Command::new(exe);
                process
                    .arg("-NoLogo")
                    .arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-Command")
                    .arg(command)
                    .current_dir(&cx.cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                match process.spawn() {
                    Ok(spawned) => {
                        child = Some(spawned);
                        break;
                    }
                    Err(error) => last_error = Some((exe, error)),
                }
            }
            let Some(mut child) = child else {
                let (exe, error) = last_error.expect("at least one executable attempted");
                return ToolOutput::err(format!("failed to spawn {exe}: {error}"));
            };
            let stdout_task = tokio::spawn(drain_tail(child.stdout.take()));
            let stderr_task = tokio::spawn(drain_tail(child.stderr.take()));

            let status =
                match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
                    Ok(Ok(status)) => status,
                    Ok(Err(error)) => {
                        let _ = child.kill().await;
                        return ToolOutput::err(format!("failed to wait for pwsh: {error}"));
                    }
                    Err(_) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return ToolOutput::err(format!("command timed out after {timeout_ms}ms"));
                    }
                };

            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();

            let mut content = String::from_utf8_lossy(&stdout).into_owned();
            if !stderr.is_empty() {
                content.push_str("\n[stderr]\n");
                content.push_str(&String::from_utf8_lossy(&stderr));
            }
            if paths::truncate_tail(&mut content, OUTPUT_CAP) {
                content.insert_str(0, "[truncated]\n");
            }
            let mut is_error = false;
            if !status.success() {
                is_error = true;
                match status.code() {
                    Some(code) => content.push_str(&format!("\n[exit code: {code}]")),
                    None => content.push_str("\n[exit code: terminated]"),
                }
            }
            ToolOutput { content, is_error }
        })
    }
}

/// Reads a child stream to the end, keeping only its last [`STREAM_KEEP_BYTES`].
async fn drain_tail<R>(reader: Option<R>) -> Vec<u8>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    let Some(mut reader) = reader else {
        return buffer;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.len() > STREAM_KEEP_BYTES * 2 {
                    let cut = buffer.len() - STREAM_KEEP_BYTES;
                    buffer.drain(..cut);
                }
            }
        }
    }
    if buffer.len() > STREAM_KEEP_BYTES {
        let cut = buffer.len() - STREAM_KEEP_BYTES;
        buffer.drain(..cut);
    }
    buffer
}
