//! The delegation and control tools the model itself can call.
//!
//! These mirror the dsh tool surface — `subagent`, `list_agents`, `send_message`,
//! `interrupt_agent`, `wait_agent` — so a model that knows how to fan out in dsh
//! needs no new vocabulary here. The same control plane is what the TUI drives,
//! which is the point: the operator and the model steer agents the same way.

use std::sync::{Arc, OnceLock, Weak};

use serde_json::{Value, json};

use destro_tools::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

use crate::agent::{AgentId, AgentStatus};
use crate::harness::{Harness, SpawnSpec};

/// Late-bound reference to the harness. Tools are built while the harness is
/// still being constructed, so the link is filled in immediately after.
pub type ToolHost = Arc<OnceLock<Weak<Harness>>>;

pub fn control_tools(host: ToolHost) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SubagentTool { host: host.clone() }),
        Arc::new(ListAgentsTool { host: host.clone() }),
        Arc::new(SendMessageTool { host: host.clone() }),
        Arc::new(InterruptTool { host: host.clone() }),
        Arc::new(WaitAgentTool { host: host.clone() }),
        Arc::new(AskUserTool { host }),
    ]
}

fn upgrade(host: &ToolHost) -> Result<Arc<Harness>, ToolOutput> {
    host.get()
        .and_then(|w| w.upgrade())
        .ok_or_else(|| ToolOutput::err("harness is not attached to this tool"))
}

fn resolve_agent(inv: &ToolInvocation) -> Result<AgentId, ToolOutput> {
    for key in ["agentId", "agent_id", "sessionId", "session_id"] {
        if let Some(s) = inv.arguments.get(key).and_then(Value::as_str) {
            return AgentId::parse(s)
                .ok_or_else(|| ToolOutput::err(format!("invalid agent id {s:?}")));
        }
    }
    Err(ToolOutput::err("missing required argument \"agentId\""))
}

fn required_str(inv: &ToolInvocation, key: &str) -> Result<String, ToolOutput> {
    inv.arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolOutput::err(format!("missing required argument {key:?}")))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn format_agent(state: &destro_proto::AgentState) -> String {
    let title = state.title.as_deref().unwrap_or("");
    let last = state
        .last_message
        .as_deref()
        .map(|m| format!(" | last: {}", truncate(&m.replace('\n', " "), 80)))
        .unwrap_or_default();
    format!(
        "{:<16} {:<8} turn {:<3} q{:<2} kids {:<2} {}{}",
        state.agent_id,
        state.status,
        state.turn,
        state.queued_messages,
        state.children,
        title,
        last
    )
}

// ---------------------------------------------------------------------------
// ask_user
// ---------------------------------------------------------------------------

pub struct AskUserTool {
    host: ToolHost,
}

impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a concise question when you need a decision, a choice or missing \
         information before you can continue. Offer `options` when the answers are a small set; \
         the user can always type their own. If the answer comes back skipped, continue with a \
         sensible default and do not ask the same thing again."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question to show." },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional suggested answers."
                }
            },
            "required": ["question"],
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let harness = match upgrade(&self.host) {
                Ok(h) => h,
                Err(e) => return e,
            };
            if !harness.interact.attached() {
                return ToolOutput::err(
                    "no interactive user is attached; make a reasonable assumption and continue",
                );
            }
            let question = match required_str(inv, "question") {
                Ok(q) => q,
                Err(e) => return e,
            };
            let options: Vec<String> = inv
                .arguments
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let agent = AgentId::parse(&cx.agent_id).unwrap_or(AgentId(0));
            let (id, rx) = harness.interact.request_question();
            harness.bus.publish(crate::bus::HarnessEvent::Question {
                id,
                agent,
                question,
                options,
            });
            match rx.await {
                Ok(Some(answer)) => ToolOutput::ok(format!("The user answered: {answer}")),
                _ => ToolOutput::ok(
                    "The user skipped this question; continue with a sensible default and do not ask it again.",
                ),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// subagent
// ---------------------------------------------------------------------------

pub struct SubagentTool {
    host: ToolHost,
}

impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn description(&self) -> &'static str {
        "Delegate a self-contained task to a child agent with a fresh context. Returns the child's final message when wait is true (default) or its id when false. Use several calls for independent research, review, or isolated work; keep writes single-threaded."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "The complete task for the child. It cannot see your conversation."},
                "title": {"type": "string", "description": "Short label for the agent list."},
                "model": {"type": "string", "description": "Optional model route for the child."},
                "wait": {"type": "boolean", "description": "Wait for the child to go idle and return its last message. Default true."}
            },
            "required": ["prompt"]
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let harness = match upgrade(&self.host) {
                Ok(h) => h,
                Err(e) => return e,
            };
            let prompt = match required_str(inv, "prompt") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let parent = match AgentId::parse(&cx.agent_id) {
                Some(p) => p,
                None => {
                    return ToolOutput::err(format!(
                        "calling agent {:?} is not addressable",
                        cx.agent_id
                    ));
                }
            };
            let wait = inv
                .arguments
                .get("wait")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let spec = SpawnSpec {
                title: inv
                    .arguments
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                model: inv
                    .arguments
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                system: None,
                cwd: Some(cx.cwd.clone()),
                prompt: Some(prompt),
                priority: None,
                history: Vec::new(),
            };
            let handle = match harness.spawn(Some(parent), spec) {
                Ok(h) => h,
                Err(e) => return ToolOutput::err(format!("spawn failed: {e}")),
            };
            let id = handle.id.wire();
            if !wait {
                return ToolOutput::ok(format!("started {id}"));
            }
            let state = handle.wait().await;
            let status = state.status.clone();
            let last = state.last_message.unwrap_or_default();
            let content = if last.trim().is_empty() {
                format!("{id} finished ({status}) with no final message")
            } else {
                format!("{id} finished ({status}):\n{last}")
            };
            if status == "failed" || status == "killed" {
                ToolOutput::err(content)
            } else {
                ToolOutput::ok(content)
            }
        })
    }
}

// ---------------------------------------------------------------------------
// list_agents
// ---------------------------------------------------------------------------

pub struct ListAgentsTool {
    host: ToolHost,
}

impl Tool for ListAgentsTool {
    fn name(&self) -> &'static str {
        "list_agents"
    }

    fn description(&self) -> &'static str {
        "List every agent in this runtime with its status, turn, queued messages, child count, and latest message. Use it to find an agent id before steering one."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent": {"type": "string", "description": "Only list descendants of this agent id."},
                "status": {"type": "string", "enum": ["idle", "running", "failed", "killed"], "description": "Only list agents in this state."}
            }
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let harness = match upgrade(&self.host) {
                Ok(h) => h,
                Err(e) => return e,
            };
            let only: Option<Vec<AgentId>> = inv
                .arguments
                .get("parent")
                .and_then(Value::as_str)
                .and_then(AgentId::parse)
                .map(|p| harness.descendants(p));
            let status = inv.arguments.get("status").and_then(Value::as_str);
            let mut rows: Vec<String> = harness
                .list()
                .into_iter()
                .filter(|s| {
                    only.as_ref()
                        .map(|set| {
                            AgentId::parse(&s.agent_id)
                                .map(|id| set.contains(&id))
                                .unwrap_or(false)
                        })
                        .unwrap_or(true)
                        && status.map(|want| s.status == want).unwrap_or(true)
                })
                .map(|s| format_agent(&s))
                .collect();
            if rows.is_empty() {
                return ToolOutput::ok("no matching agents");
            }
            if rows.len() > 200 {
                rows.truncate(200);
                rows.push("[... truncated at 200]".into());
            }
            ToolOutput::ok(rows.join("\n"))
        })
    }
}

// ---------------------------------------------------------------------------
// send_message
// ---------------------------------------------------------------------------

pub struct SendMessageTool {
    host: ToolHost,
}

impl Tool for SendMessageTool {
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn description(&self) -> &'static str {
        "Send a message to another agent. mode \"queue\" (default) delivers it at the next turn boundary; mode \"inject\" cancels the agent's in-flight request and splices the message in immediately."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agentId": {"type": "string", "description": "Target agent id, e.g. agent-000123."},
                "text": {"type": "string", "description": "Non-blank message to deliver."},
                "mode": {"type": "string", "enum": ["queue", "inject"], "description": "Delivery mode. Default queue."}
            },
            "required": ["agentId", "text"]
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let harness = match upgrade(&self.host) {
                Ok(h) => h,
                Err(e) => return e,
            };
            let id = match resolve_agent(inv) {
                Ok(id) => id,
                Err(e) => return e,
            };
            let text = match required_str(inv, "text") {
                Ok(t) => t,
                Err(e) => return e,
            };
            if text.trim().is_empty() {
                return ToolOutput::err("text must not be blank");
            }
            let mode = destro_proto::Delivery::from_wire(
                inv.arguments.get("mode").and_then(Value::as_str),
            );
            let Some(handle) = harness.get(id) else {
                return ToolOutput::err(format!("unknown agent {id}"));
            };
            match handle.send(text, mode) {
                Ok(()) => ToolOutput::ok(format!(
                    "{} to {id}",
                    match mode {
                        destro_proto::Delivery::Queue => "queued",
                        destro_proto::Delivery::Inject => "injected",
                    }
                )),
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// interrupt_agent
// ---------------------------------------------------------------------------

pub struct InterruptTool {
    host: ToolHost,
}

impl Tool for InterruptTool {
    fn name(&self) -> &'static str {
        "interrupt_agent"
    }

    fn description(&self) -> &'static str {
        "Interrupt an agent's current turn without killing it. The transcript is kept and the agent stays alive and addressable."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agentId": {"type": "string", "description": "Target agent id."}
            },
            "required": ["agentId"]
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let harness = match upgrade(&self.host) {
                Ok(h) => h,
                Err(e) => return e,
            };
            let id = match resolve_agent(inv) {
                Ok(id) => id,
                Err(e) => return e,
            };
            let Some(handle) = harness.get(id) else {
                return ToolOutput::err(format!("unknown agent {id}"));
            };
            handle.interrupt();
            ToolOutput::ok(format!("interrupt signalled to {id}"))
        })
    }
}

// ---------------------------------------------------------------------------
// wait_agent
// ---------------------------------------------------------------------------

pub struct WaitAgentTool {
    host: ToolHost,
}

impl Tool for WaitAgentTool {
    fn name(&self) -> &'static str {
        "wait_agent"
    }

    fn description(&self) -> &'static str {
        "Wait for an agent to become idle or terminal, then return its status and last message."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agentId": {"type": "string", "description": "Target agent id."}
            },
            "required": ["agentId"]
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let harness = match upgrade(&self.host) {
                Ok(h) => h,
                Err(e) => return e,
            };
            let id = match resolve_agent(inv) {
                Ok(id) => id,
                Err(e) => return e,
            };
            let Some(handle) = harness.get(id) else {
                return ToolOutput::err(format!("unknown agent {id}"));
            };
            let state = handle.wait().await;
            let last = state.last_message.clone().unwrap_or_default();
            let content = format!(
                "{id} is {} after turn {} ({} queued){}{}",
                state.status,
                state.turn,
                state.queued_messages,
                if last.is_empty() { "" } else { ":\n" },
                last
            );
            match AgentStatus::parse(&state.status) {
                Some(AgentStatus::Failed) | Some(AgentStatus::Killed) => ToolOutput::err(content),
                _ => ToolOutput::ok(content),
            }
        })
    }
}
