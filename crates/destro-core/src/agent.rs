//! One agent: state, control handle, and the loop that drives it.
//!
//! The agent is a single async task. Its mailbox carries every operator action
//! (queue a message, inject one, interrupt, swap model, kill), so steering never
//! touches the agent's internals from another thread. Everything an observer can
//! see lives in [`Live`], which is a handful of atomics plus two small mutexes —
//! reading a thousand agents must not contend with their loops.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use destro_proto::event::{MessageSource, SessionEvent, TurnEndKind, Usage};
use destro_proto::{AgentState, Delivery};
use destro_tools::{Registry, ToolContext, ToolInvocation};

use crate::bus::{EventBus, HarnessEvent};
use crate::gate::{Gate, Priority};
use crate::provider::{Provider, ProviderError, ProviderItem};

/// Mailbox depth per agent. Deep enough to absorb a burst of steering, shallow
/// enough that a dead agent cannot hold unbounded operator input.
pub const MAILBOX: usize = 64;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct AgentId(pub u64);

impl AgentId {
    /// Wire form, `agent-000123`.
    pub fn wire(&self) -> String {
        format!("agent-{:06}", self.0)
    }

    /// The session id this agent owns, `session-000123`.
    pub fn session(&self) -> Arc<str> {
        Arc::from(format!("session-{:06}", self.0))
    }

    /// Accepts `agent-000123`, `session-000123`, `000123`, or `123`.
    pub fn parse(s: &str) -> Option<AgentId> {
        s.trim()
            .trim_start_matches("agent-")
            .trim_start_matches("session-")
            .parse()
            .ok()
            .map(AgentId)
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentStatus {
    Idle,
    Running,
    Failed,
    Killed,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Running => "running",
            AgentStatus::Failed => "failed",
            AgentStatus::Killed => "killed",
        }
    }

    pub fn parse(s: &str) -> Option<AgentStatus> {
        match s {
            "idle" => Some(AgentStatus::Idle),
            "running" => Some(AgentStatus::Running),
            "failed" => Some(AgentStatus::Failed),
            "killed" => Some(AgentStatus::Killed),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            AgentStatus::Idle => 0,
            AgentStatus::Running => 1,
            AgentStatus::Failed => 2,
            AgentStatus::Killed => 3,
        }
    }

    fn from_u8(v: u8) -> AgentStatus {
        match v {
            0 => AgentStatus::Idle,
            1 => AgentStatus::Running,
            2 => AgentStatus::Failed,
            _ => AgentStatus::Killed,
        }
    }
}

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

/// The observable half of an agent. Cheap to read from any thread.
pub struct Live {
    pub parent: Option<AgentId>,
    pub created: Instant,
    pub session_id: Arc<str>,
    title: Mutex<Option<String>>,
    model: Mutex<String>,
    status: AtomicU8,
    turn: AtomicU32,
    queued: AtomicUsize,
    transcript: AtomicUsize,
    context_window: AtomicU64,
    children: AtomicUsize,
    last_tool: Mutex<Option<String>>,
    last_message: Mutex<Option<String>>,
    usage: Mutex<Usage>,
    cancel: Mutex<Option<CancellationToken>>,
    idle: Notify,
}

impl Live {
    pub(crate) fn new(
        id: AgentId,
        parent: Option<AgentId>,
        model: &str,
        title: Option<String>,
    ) -> Arc<Live> {
        Arc::new(Live {
            parent,
            created: Instant::now(),
            session_id: id.session(),
            title: Mutex::new(title),
            model: Mutex::new(model.to_string()),
            status: AtomicU8::new(AgentStatus::Idle.to_u8()),
            turn: AtomicU32::new(0),
            queued: AtomicUsize::new(0),
            transcript: AtomicUsize::new(0),
            context_window: AtomicU64::new(0),
            children: AtomicUsize::new(0),
            last_tool: Mutex::new(None),
            last_message: Mutex::new(None),
            usage: Mutex::new(Usage::default()),
            cancel: Mutex::new(None),
            idle: Notify::new(),
        })
    }

    /// The agent's cancellation token, available after spawn wires it up.
    pub(crate) fn attach_cancel(&self, cancel: CancellationToken) {
        *self.cancel.lock() = Some(cancel);
    }

    pub fn status(&self) -> AgentStatus {
        AgentStatus::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn model(&self) -> String {
        self.model.lock().clone()
    }

    pub fn set_model(&self, model: &str) {
        *self.model.lock() = model.to_string();
    }

    pub fn title(&self) -> Option<String> {
        self.title.lock().clone()
    }

    pub fn set_title(&self, title: impl Into<String>) {
        *self.title.lock() = Some(title.into());
    }

    pub fn last_message(&self) -> Option<String> {
        self.last_message.lock().clone()
    }

    pub fn usage(&self) -> Usage {
        *self.usage.lock()
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub fn children(&self) -> usize {
        self.children.load(Ordering::Relaxed)
    }

    pub fn transcript_len(&self) -> usize {
        self.transcript.load(Ordering::Relaxed)
    }

    pub fn turn(&self) -> u32 {
        self.turn.load(Ordering::Relaxed)
    }

    pub(crate) fn add_usage(&self, delta: &Usage) {
        self.usage.lock().accumulate(delta);
    }

    fn set_status(&self, status: AgentStatus) -> bool {
        let prev = self.status.swap(status.to_u8(), Ordering::AcqRel);
        let changed = prev != status.to_u8();
        if changed && !matches!(status, AgentStatus::Running) {
            self.idle.notify_waiters();
        }
        changed
    }

    pub(crate) fn bump_children(&self, delta: isize) {
        if delta >= 0 {
            self.children.fetch_add(delta as usize, Ordering::Relaxed);
        } else {
            self.children
                .fetch_sub((-delta) as usize, Ordering::Relaxed);
        }
    }

    pub(crate) fn bump_transcript(&self) -> usize {
        self.transcript.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Wait until the agent is not running and has no queued message.
    ///
    /// A cancelled agent is not settled just because it happens to be idle at
    /// this instant: cancellation may be propagating from an ancestor, so wait
    /// for the loop to reach a terminal status.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            let status = self.status();
            let terminal = matches!(status, AgentStatus::Failed | AgentStatus::Killed);
            let cancelled = self
                .cancel
                .lock()
                .as_ref()
                .map(|c| c.is_cancelled())
                .unwrap_or(false);
            if terminal || (status != AgentStatus::Running && self.queued() == 0 && !cancelled) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn snapshot(&self, id: AgentId) -> AgentState {
        let usage = *self.usage.lock();
        AgentState {
            agent_id: id.wire(),
            session_id: self.session_id.to_string(),
            parent_id: self.parent.map(|p| p.wire()),
            title: self.title(),
            model: self.model(),
            status: self.status().as_str().to_string(),
            turn: self.turn.load(Ordering::Relaxed),
            queued_messages: self.queued.load(Ordering::Relaxed),
            children: self.children.load(Ordering::Relaxed),
            transcript_len: self.transcript.load(Ordering::Relaxed),
            last_tool: self.last_tool.lock().clone(),
            usage,
            context_window: match self.context_window.load(Ordering::Relaxed) {
                0 => None,
                w => Some(w),
            },
            created_ms_ago: self.created.elapsed().as_millis() as u64,
            last_message: self.last_message(),
        }
    }
}

// ---------------------------------------------------------------------------
// Control handle
// ---------------------------------------------------------------------------

pub(crate) enum Command {
    Send { text: String, mode: Delivery },
    Interrupt,
    SetModel(String),
    Kill,
    State(oneshot::Sender<AgentState>),
}

/// A cheap, cloneable handle used for inspection and steering.
#[derive(Clone)]
pub struct AgentHandle {
    pub id: AgentId,
    pub live: Arc<Live>,
    pub cancel: CancellationToken,
    pub priority: Priority,
    tx: mpsc::Sender<Command>,
}

impl AgentHandle {
    pub(crate) fn new(
        id: AgentId,
        live: Arc<Live>,
        cancel: CancellationToken,
        priority: Priority,
        tx: mpsc::Sender<Command>,
    ) -> AgentHandle {
        AgentHandle {
            id,
            live,
            cancel,
            priority,
            tx,
        }
    }

    /// Queue or inject a message. `Inject` preempts the in-flight request.
    pub fn send(&self, text: impl Into<String>, mode: Delivery) -> Result<(), &'static str> {
        self.tx
            .try_send(Command::Send {
                text: text.into(),
                mode,
            })
            .map_err(|_| "agent mailbox is full")?;
        self.live.queued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Abort the current model request or tool, keeping the transcript and the agent.
    pub fn interrupt(&self) {
        let _ = self.tx.try_send(Command::Interrupt);
    }

    pub fn set_model(&self, model: impl Into<String>) {
        let _ = self.tx.try_send(Command::SetModel(model.into()));
    }

    /// Stop the agent and every descendant.
    pub fn kill(&self) {
        self.cancel.cancel();
        let _ = self.tx.try_send(Command::Kill);
    }

    pub fn status(&self) -> AgentStatus {
        self.live.status()
    }

    pub fn is_running(&self) -> bool {
        self.live.status() == AgentStatus::Running
    }

    /// A point-in-time inspector view.
    pub async fn state(&self) -> AgentState {
        let (tx, rx) = oneshot::channel();
        if self.tx.try_send(Command::State(tx)).is_ok() {
            if let Ok(state) = rx.await {
                return state;
            }
        }
        // The task is gone; report what the shared cell still knows.
        self.live.snapshot(self.id)
    }

    /// Block until the agent is idle (or terminal) and report its final state.
    pub async fn wait(&self) -> AgentState {
        self.live.wait_idle().await;
        self.state().await
    }
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

pub(crate) struct AgentInit {
    pub id: AgentId,
    pub live: Arc<Live>,
    pub parent_live: Option<Arc<Live>>,
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<Registry>,
    pub gate: Arc<Gate>,
    pub bus: EventBus,
    pub cwd: PathBuf,
    pub system: String,
    pub model: String,
    pub max_tokens: Option<u64>,
    pub max_steps: u32,
    pub priority: Priority,
}

pub(crate) struct AgentTask {
    id: AgentId,
    live: Arc<Live>,
    parent_live: Option<Arc<Live>>,
    provider: Arc<dyn Provider>,
    tools: Arc<Registry>,
    gate: Arc<Gate>,
    bus: EventBus,
    cwd: PathBuf,
    system: String,
    model: String,
    max_tokens: Option<u64>,
    max_steps: u32,
    priority: Priority,
    cancel: CancellationToken,
    messages: Vec<Value>,
    turn: u32,
}

struct TurnOutcome {
    kind: TurnEndKind,
    notice: Option<String>,
}

impl AgentTask {
    pub(crate) fn new(init: AgentInit, cancel: CancellationToken) -> AgentTask {
        AgentTask {
            id: init.id,
            live: init.live,
            parent_live: init.parent_live,
            provider: init.provider,
            tools: init.tools,
            gate: init.gate,
            bus: init.bus,
            cwd: init.cwd,
            system: init.system,
            model: init.model,
            max_tokens: init.max_tokens,
            max_steps: init.max_steps,
            priority: init.priority,
            cancel,
            messages: Vec::new(),
            turn: 0,
        }
    }

    pub(crate) async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut pending: VecDeque<String> = VecDeque::new();
        let mut kill_requested = false;
        let mut failed = false;

        loop {
            if self.cancel.is_cancelled() {
                kill_requested = true;
                break;
            }
            if pending.is_empty() {
                // Wake on cancellation too: an idle agent whose tree was killed
                // would otherwise sit in `recv` forever, because no message comes.
                let cmd = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => {
                        kill_requested = true;
                        break;
                    }
                    cmd = rx.recv() => cmd,
                };
                match cmd {
                    None => break,
                    Some(cmd) => match cmd {
                        Command::Send { text, .. } => pending.push_back(text),
                        Command::Interrupt => {}
                        Command::Kill => {
                            self.cancel.cancel();
                            kill_requested = true;
                            break;
                        }
                        Command::SetModel(model) => self.switch_model(model),
                        Command::State(tx) => {
                            let _ = tx.send(self.live.snapshot(self.id));
                        }
                    },
                }
                if pending.is_empty() {
                    continue;
                }
            }

            // Commit everything queued as this turn's input.
            let claimed = pending.len();
            while let Some(text) = pending.pop_front() {
                self.commit_user_message(&text);
            }
            self.live
                .queued
                .fetch_sub(claimed.min(self.live.queued()), Ordering::Relaxed);

            // ----- one turn ---------------------------------------------------
            self.turn += 1;
            let turn_no = self.turn;
            self.live.turn.store(turn_no, Ordering::Relaxed);
            self.emit(SessionEvent::TurnStart { turn: turn_no });
            self.set_status(AgentStatus::Running);

            let turn_cancel = self.cancel.child_token();
            let cancel = self.cancel.clone();
            let id = self.id;
            let live = self.live.clone();
            let mut inbox: Vec<Command> = Vec::new();
            let outcome = {
                let fut = self.drive_turn(&turn_cancel);
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        biased;
                        r = &mut fut => break r,
                        cmd = rx.recv() => match cmd {
                            None => {
                                cancel.cancel();
                                turn_cancel.cancel();
                            }
                            Some(Command::Interrupt) => turn_cancel.cancel(),
                            Some(Command::Send { text, mode }) => {
                                if mode == Delivery::Inject {
                                    turn_cancel.cancel();
                                }
                                inbox.push(Command::Send { text, mode });
                            }
                            Some(Command::Kill) => {
                                cancel.cancel();
                                turn_cancel.cancel();
                                inbox.push(Command::Kill);
                            }
                            Some(Command::SetModel(m)) => inbox.push(Command::SetModel(m)),
                            Some(Command::State(tx)) => {
                                let _ = tx.send(live.snapshot(id));
                            }
                        }
                    }
                }
            };

            self.emit(SessionEvent::TurnEnd {
                turn: turn_no,
                kind: outcome.kind,
            });
            if let Some(text) = outcome.notice {
                self.emit(SessionEvent::Notice { text });
            }

            for cmd in inbox.drain(..) {
                match cmd {
                    Command::Send { text, .. } => pending.push_back(text),
                    Command::SetModel(m) => self.switch_model(m),
                    Command::Kill => {
                        self.cancel.cancel();
                        kill_requested = true;
                    }
                    _ => {}
                }
            }
            // Queued-message accounting stays symmetric: `send` increments when a
            // message enters the mailbox, the claim above decrements, and merging
            // the inbox changes nothing (those sends were already counted).

            if kill_requested || self.cancel.is_cancelled() {
                kill_requested = true;
                break;
            }
            if outcome.kind == TurnEndKind::Failed {
                failed = true;
                break;
            }
            // No queued work: park in the mailbox and report idle to observers
            // before blocking, so `wait()` never sees a stuck "running" agent.
            if pending.is_empty() {
                self.set_status(AgentStatus::Idle);
            }
        }

        let status = if kill_requested {
            AgentStatus::Killed
        } else if failed {
            AgentStatus::Failed
        } else {
            AgentStatus::Idle
        };
        self.set_status(status);
        if let Some(parent) = &self.parent_live {
            parent.bump_children(-1);
        }
        self.bus.publish(HarnessEvent::Finished {
            agent: self.id,
            status,
            last_message: self.live.last_message(),
        });
    }

    /// Record a queued user message in the transcript and the conversation.
    fn commit_user_message(&mut self, text: &str) {
        if self.live.title().is_none() {
            let title = first_line(text, 48);
            self.live.set_title(title.clone());
            self.emit(SessionEvent::SessionTitle { title });
        }
        self.emit(SessionEvent::UserMessage {
            text: text.to_string(),
            source: if self.live.parent.is_some() {
                MessageSource::Subagent
            } else {
                MessageSource::User
            },
        });
        self.messages
            .push(json!({ "role": "user", "content": text }));
    }

    fn switch_model(&mut self, model: String) {
        self.live.set_model(&model);
        self.model = model;
    }

    fn set_status(&self, status: AgentStatus) {
        if self.live.set_status(status) {
            self.bus.publish(HarnessEvent::Status {
                agent: self.id,
                status,
            });
        }
    }

    fn emit(&self, event: SessionEvent) {
        self.live.bump_transcript();
        self.bus.session(self.id, &self.live.session_id, event);
    }

    /// Drive model requests and tool calls until the turn ends.
    async fn drive_turn(&mut self, cancel: &CancellationToken) -> TurnOutcome {
        let mut steps = 0u32;
        loop {
            if cancel.is_cancelled() {
                return TurnOutcome {
                    kind: TurnEndKind::Interrupted,
                    notice: None,
                };
            }
            steps += 1;
            if steps > self.max_steps {
                return TurnOutcome {
                    kind: TurnEndKind::Failed,
                    notice: Some(format!("step limit {} reached", self.max_steps)),
                };
            }

            let request = crate::provider::ChatRequest {
                model: self.model.clone(),
                system: self.system.clone(),
                messages: self.messages.clone(),
                tools: self.tools.schemas(),
                max_tokens: self.max_tokens,
            };

            // Hold a provider slot only while the request is live.
            let gate = self.gate.clone();
            let permit = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return TurnOutcome { kind: TurnEndKind::Interrupted, notice: None };
                }
                permit = gate.acquire(self.priority) => permit,
            };

            let mut stream = self.provider.stream(request, cancel.clone());
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls: Vec<ProviderItem> = Vec::new();
            let mut usage = Usage::default();
            let mut error: Option<String> = None;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    item = stream.next() => match item {
                        None => break,
                        Some(Ok(ProviderItem::TextDelta(t))) => {
                            self.bus.delta(self.id, false, &t);
                            text.push_str(&t);
                        }
                        Some(Ok(ProviderItem::ReasoningDelta(t))) => {
                            self.bus.delta(self.id, true, &t);
                            reasoning.push_str(&t);
                        }
                        Some(Ok(ProviderItem::ToolCall { id, name, arguments })) => {
                            calls.push(ProviderItem::ToolCall { id, name, arguments });
                        }
                        Some(Ok(ProviderItem::Usage(u))) => usage.accumulate(&u),
                        Some(Ok(ProviderItem::Done { .. })) => {}
                        Some(Err(ProviderError::Cancelled)) => break,
                        Some(Err(e)) => {
                            error = Some(e.to_string());
                            break;
                        }
                    }
                }
            }
            drop(permit);

            let interrupted = cancel.is_cancelled();
            if !text.is_empty() {
                *self.live.last_message.lock() = Some(text.clone());
            }
            if !usage_flat(&usage) {
                self.live.add_usage(&usage);
            }
            if !text.is_empty() || !reasoning.is_empty() || !calls.is_empty() || interrupted {
                self.emit(SessionEvent::AssistantMessage {
                    text: text.clone(),
                    reasoning: (!reasoning.is_empty()).then(|| reasoning.clone()),
                    usage: Some(usage),
                    interrupted,
                });
            }

            // Commit the assistant message to the conversation.
            let mut message = json!({ "role": "assistant", "content": text });
            if !calls.is_empty() {
                let encoded: Vec<Value> = calls
                    .iter()
                    .filter_map(|c| match c {
                        ProviderItem::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "id": id, "type": "function",
                            "function": { "name": name, "arguments": arguments }
                        })),
                        _ => None,
                    })
                    .collect();
                message["tool_calls"] = Value::Array(encoded);
            }
            self.messages.push(message);

            if let Some(err) = error {
                return TurnOutcome {
                    kind: TurnEndKind::Failed,
                    notice: Some(format!("provider error: {err}")),
                };
            }
            if interrupted {
                return TurnOutcome {
                    kind: TurnEndKind::Interrupted,
                    notice: None,
                };
            }
            if calls.is_empty() {
                return TurnOutcome {
                    kind: TurnEndKind::Done,
                    notice: None,
                };
            }

            // ----- tools ------------------------------------------------------
            for call in calls {
                let ProviderItem::ToolCall {
                    id: call_id,
                    name,
                    arguments,
                } = call
                else {
                    continue;
                };
                self.emit(SessionEvent::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                *self.live.last_tool.lock() = Some(name.clone());

                let (content, is_error) =
                    match self.run_tool(&name, &call_id, &arguments, cancel).await {
                        Ok(pair) => pair,
                        Err(ToolFail::Interrupted) => {
                            self.emit(SessionEvent::ToolResult {
                                call_id: call_id.clone(),
                                name,
                                is_error: true,
                                content: "interrupted by operator".into(),
                            });
                            return TurnOutcome {
                                kind: TurnEndKind::Interrupted,
                                notice: None,
                            };
                        }
                        Err(ToolFail::Invalid(msg)) => {
                            let text = format!("invalid arguments: {msg}");
                            self.emit(SessionEvent::ToolResult {
                                call_id: call_id.clone(),
                                name: name.clone(),
                                is_error: true,
                                content: text.clone(),
                            });
                            self.messages.push(json!({
                            "role": "tool", "tool_call_id": call_id, "name": name, "content": text
                        }));
                            continue;
                        }
                    };

                self.emit(SessionEvent::ToolResult {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    is_error,
                    content: content.clone(),
                });
                self.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "name": name,
                    "content": content,
                }));
            }
        }
    }

    async fn run_tool(
        &self,
        name: &str,
        call_id: &str,
        arguments: &str,
        cancel: &CancellationToken,
    ) -> Result<(String, bool), ToolFail> {
        let Some(tool) = self.tools.get(name) else {
            return Ok((format!("error: unknown tool {name:?}"), true));
        };
        let args: Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return Err(ToolFail::Invalid(e.to_string())),
        };
        let invocation = ToolInvocation {
            call_id: call_id.to_string(),
            arguments: args,
        };
        let context = ToolContext {
            cwd: self.cwd.clone(),
            agent_id: self.id.wire(),
        };
        let fut = tool.run(&invocation, &context);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolFail::Interrupted),
            out = fut => Ok(if out.is_error { (format!("error: {}", out.content), true) } else { (out.content, false) }),
        }
    }
}

enum ToolFail {
    Interrupted,
    Invalid(String),
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut out: String = line.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn usage_flat(u: &Usage) -> bool {
    u.input_tokens == 0
        && u.output_tokens == 0
        && u.cache_read_tokens == 0
        && u.cache_write_tokens == 0
        && u.reasoning_tokens == 0
}

/// Build the first user-role message of a conversation.
pub fn first_user_message(text: &str) -> (SessionEvent, Value) {
    (
        SessionEvent::UserMessage {
            text: text.to_string(),
            source: MessageSource::User,
        },
        json!({ "role": "user", "content": text }),
    )
}
