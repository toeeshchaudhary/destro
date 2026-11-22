//! Pure state: projects (one `dsh --profile sdk` runtime each), sessions, and the
//! projection of the harness's `session.event` log into append-only transcript items.
//!
//! Memory policy: transcript items are append-only, tool output is truncated on
//! ingest, and each session is capped at `MAX_ITEMS` (oldest items are dropped in
//! batches — the harness keeps the full durable log, we only hold a window).

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

pub const MAX_ITEMS: usize = 20_000;
const DROP_BATCH: usize = 2_000;
const MAX_TOOL_TEXT: usize = 4_096;
const MAX_ARGS_TEXT: usize = 600;

#[derive(Clone, Debug)]
pub enum Item {
    User(String),
    Assistant(String),
    Reasoning(String),
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        is_error: bool,
        text: String,
        lang: Option<String>,
    },
    Notice(String),
}

/// A code block or file body worth keeping for the snippet viewer.
#[derive(Clone)]
pub struct Snippet {
    /// Global creation order, used to merge snippets across an agent tree.
    pub id: u64,
    /// Session that produced it.
    pub from: String,
    pub title: String,
    pub lang: String,
    pub code: String,
}

const MAX_SNIPPETS: usize = 100;
static SNIPPET_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const MAX_SNIPPET_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Running,
    Failed,
}

#[derive(Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub reasoning: u64,
    /// Uncached + cached input of the most recent request: the live context size.
    pub last_context: u64,
}

pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub parent: Option<String>,
    pub status: Status,
    pub items: VecDeque<Item>,
    pub dropped: usize,
    pub turn: u32,
    pub usage: Usage,
    pub context_window: Option<u64>,
    pub unread: bool,
    pub last_tool: Option<String>,
    /// Lines scrolled up from the bottom; 0 = follow the tail.
    pub scroll: usize,
    /// call id -> (tool name, file path argument if any)
    pub tool_names: HashMap<String, (String, Option<String>)>,
    pub snippets: Vec<Snippet>,
    pub cache: crate::ui::LineCache,
    /// When the current turn began (drives the elapsed-time readout).
    pub started: Option<std::time::Instant>,
}

impl Session {
    pub fn new(id: &str, parent: Option<String>) -> Self {
        Session {
            id: id.to_string(),
            title: None,
            parent,
            status: Status::Idle,
            items: VecDeque::new(),
            dropped: 0,
            turn: 0,
            usage: Usage::default(),
            context_window: None,
            unread: false,
            last_tool: None,
            scroll: 0,
            tool_names: HashMap::new(),
            snippets: vec![],
            cache: Default::default(),
            started: None,
        }
    }

    pub fn label(&self) -> String {
        match &self.title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => {
                let short: String = self.id.chars().take(10).collect();
                format!("session {short}")
            }
        }
    }

    fn add_snippet(&mut self, title: String, lang: String, code: String) {
        if code.trim().is_empty() {
            return;
        }
        let code = if code.len() > MAX_SNIPPET_BYTES {
            truncate(&code, MAX_SNIPPET_BYTES)
        } else {
            code
        };
        let id = SNIPPET_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.snippets.push(Snippet {
            id,
            from: self.id.clone(),
            title,
            lang,
            code,
        });
        if self.snippets.len() > MAX_SNIPPETS {
            self.snippets.remove(0);
        }
    }

    fn push(&mut self, item: Item) {
        self.items.push_back(item);
        if self.items.len() > MAX_ITEMS {
            for _ in 0..DROP_BATCH {
                self.items.pop_front();
            }
            self.dropped += DROP_BATCH;
            self.cache.invalidate();
        }
    }

    /// Fold one committed session-log event (the `event` of a `session.event`
    /// notification) into the transcript. Unknown event types are ignored, which
    /// matches the harness rule that readers may skip `ignorable` records.
    pub fn apply_event(&mut self, event: &Value) {
        let ty = event["type"].as_str().unwrap_or("");
        let data = &event["data"];
        match ty {
            "turn/start" => self.turn = data["turn"].as_u64().unwrap_or(0) as u32,
            "turn/end" => {
                let kind = data["reason"]["kind"].as_str().unwrap_or("done");
                if kind != "stop" && kind != "done" && kind != "end-turn" {
                    self.push(Item::Notice(format!("turn {} ended: {kind}", self.turn)));
                }
            }
            "user/message" => {
                // synthetic injections (file notices, cron, skills) are not human prompts
                let text = blocks_text(&data["content"]);
                let kind = data["source"]["kind"].as_str().unwrap_or("user");
                if kind == "user" || kind.is_empty() {
                    if self.title.is_none() {
                        self.title =
                            Some(text.chars().take(48).collect::<String>().replace('\n', " "));
                    }
                    self.push(Item::User(text));
                } else if !text.is_empty() {
                    if self.title.is_none() && kind == "subagent" {
                        self.title = Some(first_line(&text, 48));
                    }
                    self.push(Item::Notice(format!("{kind}: {}", first_line(&text, 100))));
                }
            }
            "assistant/message" => {
                let msg = &data["message"];
                if let Some(arr) = msg["content"].as_array() {
                    for b in arr {
                        match b["type"].as_str() {
                            Some("reasoning") => {
                                self.push(Item::Reasoning(b["text"].as_str().unwrap_or("").into()))
                            }
                            Some("text") => {
                                let text = b["text"].as_str().unwrap_or("");
                                for (lang, code) in crate::md::fences(text) {
                                    let title = if lang.is_empty() {
                                        "code".to_string()
                                    } else {
                                        lang.clone()
                                    };
                                    self.add_snippet(title, lang, code);
                                }
                                self.push(Item::Assistant(text.into()))
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(u) = data.get("usage").filter(|u| u.is_object()) {
                    let g = |k: &str| u[k].as_u64().unwrap_or(0);
                    self.usage.input += g("inputTokens");
                    self.usage.output += g("outputTokens");
                    self.usage.cache_read += g("cacheReadTokens");
                    self.usage.reasoning += g("reasoningTokens");
                    self.usage.last_context =
                        g("inputTokens") + g("cacheReadTokens") + g("cacheWriteTokens");
                }
                if data["interrupted"].as_bool() == Some(true) {
                    self.push(Item::Notice("interrupted".into()));
                }
            }
            "tool/call" => {
                let name = data["name"].as_str().unwrap_or("?").to_string();
                let call_id = data["callId"].as_str().unwrap_or("").to_string();
                let path = serde_json::from_str::<Value>(data["arguments"].as_str().unwrap_or(""))
                    .ok()
                    .and_then(|a| {
                        ["path", "file_path", "file", "filename"]
                            .iter()
                            .find_map(|k| a[*k].as_str().map(str::to_string))
                    });
                self.tool_names.insert(call_id, (name.clone(), path));
                self.last_tool = Some(name.clone());
                let args = truncate(data["arguments"].as_str().unwrap_or(""), MAX_ARGS_TEXT);
                self.push(Item::ToolCall { name, args });
            }
            "tool/result" => {
                let m = &data["message"];
                let id = m["toolCallId"].as_str().unwrap_or("");
                let (_name, path) = self.tool_names.remove(id).unwrap_or(("tool".into(), None));
                let is_error = m["isError"].as_bool().unwrap_or(false);
                let full = blocks_text(&m["content"]);
                let looks_diff = full.lines().any(|l| l.starts_with("@@ "))
                    || (full.lines().any(|l| l.starts_with("+++ "))
                        && full.lines().any(|l| l.starts_with("--- ")));
                let ext = path
                    .as_deref()
                    .and_then(|p| p.rsplit_once('.'))
                    .map(|(_, e)| e.to_string());
                let lang = if is_error {
                    None
                } else if looks_diff {
                    Some("diff".to_string())
                } else {
                    ext.filter(|e| crate::highlight::is_known(e))
                };
                let is_file = path.as_deref().is_some_and(|p| {
                    p.rsplit(['/', '\\'])
                        .next()
                        .is_some_and(|f| f.contains('.'))
                });
                if !is_error && (looks_diff || (is_file && full.lines().count() >= 3)) {
                    let title = path.clone().unwrap_or_else(|| "diff".into());
                    self.add_snippet(title, lang.clone().unwrap_or_default(), full.clone());
                }
                let text = truncate(&full, MAX_TOOL_TEXT);
                self.push(Item::ToolResult {
                    is_error,
                    text,
                    lang,
                });
            }
            "compaction/start" => self.push(Item::Notice("compacting context…".into())),
            "compaction/end" => {
                self.usage.last_context = 0;
                self.push(Item::Notice("context compacted".into()));
            }
            "session/title" => {
                if let Some(t) = data["title"].as_str() {
                    self.title = Some(t.to_string());
                }
            }
            "request/context" => {
                if let Some(w) = data["contextWindow"].as_u64() {
                    self.context_window = Some(w);
                }
            }
            _ => {}
        }
    }

    pub fn ctx_ratio(&self) -> Option<f64> {
        let w = self.context_window? as f64;
        Some((self.usage.last_context as f64 / w).clamp(0.0, 1.0))
    }
}

pub struct Approval {
    pub rpc_id: Value,
    pub session_id: String,
    pub tool: String,
    pub reason: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conn {
    Starting,
    Ready,
    Dead,
}

pub struct Project {
    pub name: String,
    pub cwd: String,
    pub model: String,
    pub conn: Conn,
    pub sessions: Vec<Session>,
    pub index: HashMap<String, usize>,
    pub approvals: VecDeque<Approval>,
    pub log: VecDeque<String>,
    pub collapsed: bool,
    pub closed: bool,
    pub skills: Vec<destro_core::skills::Skill>,
    pub commands: Vec<destro_core::skills::CustomCommand>,
}

impl Project {
    pub fn new(name: &str, cwd: &str, model: &str) -> Self {
        Project {
            name: name.into(),
            cwd: cwd.into(),
            model: model.into(),
            conn: Conn::Starting,
            sessions: vec![],
            index: HashMap::new(),
            approvals: VecDeque::new(),
            log: VecDeque::new(),
            collapsed: false,
            closed: false,
            skills: vec![],
            commands: vec![],
        }
    }

    /// Discover skills and custom commands for this workspace.
    pub fn load_conventions(&mut self) {
        let root = std::path::PathBuf::from(&self.cwd);
        self.skills = destro_core::skills::discover_skills(&root);
        self.commands = destro_core::skills::discover_commands(&root);
    }

    pub fn session_mut(&mut self, id: &str) -> usize {
        if let Some(&i) = self.index.get(id) {
            return i;
        }
        self.sessions.push(Session::new(id, None));
        let i = self.sessions.len() - 1;
        self.index.insert(id.to_string(), i);
        i
    }

    pub fn note(&mut self, line: String) {
        self.log.push_back(line);
        if self.log.len() > 200 {
            self.log.pop_front();
        }
    }

    pub fn children_of(&self, id: &str) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.parent.as_deref() == Some(id))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn running_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.status == Status::Running)
            .count()
    }
}

fn blocks_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_string),
                Some("image") => Some("[image]".into()),
                Some("file") => Some("[file]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn first_line(s: &str, max: usize) -> String {
    truncate(s.lines().next().unwrap_or(""), max)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}
