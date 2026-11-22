//! Model Context Protocol client: connect to MCP servers and expose their tools.
//!
//! Servers are configured in `mcp.json` files using the same shape other agent
//! tools use, so an existing `.mcp.json` works as is:
//!
//! ```json
//! { "mcpServers": {
//!     "files":  { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
//!     "remote": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${TOKEN}" } }
//! } }
//! ```
//!
//! Files read, later overriding earlier by server name: `~/.destro/mcp.json`
//! (user scope), then `.mcp.json` and `.destro/mcp.json` in every directory
//! from the filesystem root down to the workspace (project scope).
//!
//! Project-scope servers launch arbitrary commands, so each one waits for the
//! user's approval once (`McpHub::approve`); the choice is remembered in
//! `mcp-trust.json`. User-scope servers start automatically.
//!
//! Transports: stdio (newline-delimited JSON-RPC) and streamable HTTP. Tools are
//! registered as `mcp__<server>__<tool>`. Resources and prompts are not used yet.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use destro_tools::{BoxFuture, Registry, Tool, ToolContext, ToolInvocation, ToolOutput};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

use crate::context::{Scope, ancestors_root_first, data_dir, destro_home};

const PROTOCOL: &str = "2025-06-18";
const INIT_TIMEOUT: Duration = Duration::from_secs(40);
const CALL_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_OUTPUT: usize = 100_000;

// ----- configuration ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub source: PathBuf,
    pub scope: Scope,
    pub disabled: bool,
}

impl ServerConfig {
    /// One-line description for UIs: the command line or URL.
    pub fn detail(&self) -> String {
        match &self.transport {
            Transport::Stdio { command, args, .. } => {
                let mut s = command.clone();
                for a in args {
                    s.push(' ');
                    s.push_str(a);
                }
                s
            }
            Transport::Http { url, .. } => url.clone(),
        }
    }

    fn fingerprint(&self) -> String {
        format!("{}|{}", self.name, self.detail())
    }
}

/// `${VAR}` and `${VAR:-default}` expansion.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let spec = &after[..j];
        let (var, default) = match spec.split_once(":-") {
            Some((v, d)) => (v, Some(d)),
            None => (spec, None),
        };
        match std::env::var(var) {
            Ok(v) => out.push_str(&v),
            Err(_) => out.push_str(default.unwrap_or("")),
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

fn parse_servers(text: &str, source: &Path, scope: Scope) -> Vec<ServerConfig> {
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return vec![];
    };
    let Some(map) = root["mcpServers"]
        .as_object()
        .or_else(|| root["servers"].as_object())
    else {
        return vec![];
    };
    let mut out = vec![];
    for (name, v) in map {
        let disabled =
            v["disabled"].as_bool() == Some(true) || v["enabled"].as_bool() == Some(false);
        let pairs = |key: &str| -> Vec<(String, String)> {
            v[key]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), expand_env(s))))
                        .collect()
                })
                .unwrap_or_default()
        };
        let transport = if let Some(cmd) = v["command"].as_str() {
            Transport::Stdio {
                command: expand_env(cmd),
                args: v["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(expand_env))
                            .collect()
                    })
                    .unwrap_or_default(),
                env: pairs("env"),
                cwd: v["cwd"].as_str().map(|c| PathBuf::from(expand_env(c))),
            }
        } else if let Some(url) = v["url"].as_str().or_else(|| v["serverUrl"].as_str()) {
            Transport::Http {
                url: expand_env(url),
                headers: pairs("headers"),
            }
        } else {
            continue;
        };
        out.push(ServerConfig {
            name: name.clone(),
            transport,
            source: source.to_path_buf(),
            scope,
            disabled,
        });
    }
    out
}

/// Every configured server visible from `cwd`, sorted by name.
pub fn load_configs(cwd: &Path) -> Vec<ServerConfig> {
    let mut by_name: std::collections::BTreeMap<String, ServerConfig> = Default::default();
    let mut files: Vec<(Scope, PathBuf)> = vec![];
    if let Some(d) = destro_home() {
        files.push((Scope::User, d.join("mcp.json")));
    }
    for dir in ancestors_root_first(cwd) {
        files.push((Scope::Project, dir.join(".mcp.json")));
        files.push((Scope::Project, dir.join(".destro").join("mcp.json")));
    }
    for (scope, path) in files {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for cfg in parse_servers(&text, &path, scope) {
                by_name.insert(cfg.name.clone(), cfg);
            }
        }
    }
    by_name.into_values().collect()
}

// ----- trust store -----------------------------------------------------------------------

fn trust_file() -> Option<PathBuf> {
    data_dir().map(|d| d.join("mcp-trust.json"))
}

fn trusted() -> Vec<String> {
    trust_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn remember_trust(fp: &str) {
    let Some(p) = trust_file() else { return };
    let mut all = trusted();
    if !all.iter().any(|x| x == fp) {
        all.push(fp.to_string());
    }
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(t) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::write(p, t);
    }
}

// ----- JSON-RPC clients ------------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct StdioClient {
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next: AtomicU64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    _child: Child,
}

struct HttpClient {
    http: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Option<String>>,
    next: AtomicU64,
}

enum Conn {
    Stdio(Box<StdioClient>),
    Http(Box<HttpClient>),
}

/// A live connection to one server.
pub struct Client {
    conn: Conn,
}

fn build_command(command: &str, args: &[String]) -> Command {
    let wrap = cfg!(windows) && !command.to_lowercase().ends_with(".exe");
    let mut cmd = if wrap {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        Command::new(command)
    };
    cmd.args(args);
    cmd
}

impl Client {
    async fn connect(cfg: &ServerConfig, cwd: &Path) -> Result<Client, String> {
        let conn = match &cfg.transport {
            Transport::Stdio {
                command,
                args,
                env,
                cwd: dir,
            } => {
                let mut cmd = build_command(command, args);
                cmd.current_dir(dir.clone().unwrap_or_else(|| cwd.to_path_buf()))
                    .envs(env.iter().map(|(k, v)| (k, v)))
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                #[cfg(windows)]
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                let mut child = cmd
                    .spawn()
                    .map_err(|e| format!("cannot start `{command}`: {e}"))?;
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = child.stdout.take().ok_or("no stdout")?;
                let stderr = child.stderr.take().ok_or("no stderr")?;
                let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
                let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
                let tail: Arc<Mutex<VecDeque<String>>> = Default::default();
                {
                    let tail = tail.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(l)) = lines.next_line().await {
                            let mut t = tail.lock();
                            if t.len() >= 4 {
                                t.pop_front();
                            }
                            t.push_back(l);
                        }
                    });
                }
                {
                    let pending = pending.clone();
                    let stdin = stdin.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            let id = v.get("id").cloned().filter(|i| !i.is_null());
                            if v.get("method").is_some() {
                                // a request from the server: answer ping, refuse the rest
                                if let Some(id) = id {
                                    let reply = if v["method"] == "ping" {
                                        json!({"jsonrpc":"2.0","id":id,"result":{}})
                                    } else {
                                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not supported"}})
                                    };
                                    let mut w = stdin.lock().await;
                                    let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
                                    let _ = w.flush().await;
                                }
                                continue;
                            }
                            if let Some(n) = id.and_then(|i| i.as_u64())
                                && let Some(tx) = pending.lock().remove(&n)
                            {
                                let _ = tx.send(match v.get("error") {
                                    Some(e) => Err(e["message"]
                                        .as_str()
                                        .unwrap_or("server error")
                                        .to_string()),
                                    None => Ok(v["result"].clone()),
                                });
                            }
                        }
                        for (_, tx) in pending.lock().drain() {
                            let _ = tx.send(Err("server exited".into()));
                        }
                    });
                }
                Conn::Stdio(Box::new(StdioClient {
                    stdin,
                    pending,
                    next: AtomicU64::new(1),
                    stderr_tail: tail,
                    _child: child,
                }))
            }
            Transport::Http { url, headers } => Conn::Http(Box::new(HttpClient {
                http: reqwest::Client::new(),
                url: url.clone(),
                headers: headers.clone(),
                session: Mutex::new(None),
                next: AtomicU64::new(1),
            })),
        };
        let client = Client { conn };
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL,
                    "capabilities": {},
                    "clientInfo": { "name": "destro", "version": env!("CARGO_PKG_VERSION") },
                }),
                INIT_TIMEOUT,
            )
            .await?;
        client.notify("notifications/initialized", json!({})).await;
        Ok(client)
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        match &self.conn {
            Conn::Stdio(c) => {
                let id = c.next.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = oneshot::channel();
                c.pending.lock().insert(id, tx);
                let line = format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                );
                {
                    let mut w = c.stdin.lock().await;
                    if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                        c.pending.lock().remove(&id);
                        return Err(self.exit_reason("server closed its input"));
                    }
                }
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(r)) => r.map_err(|e| {
                        if e == "server exited" {
                            self.exit_reason(&e)
                        } else {
                            e
                        }
                    }),
                    Ok(Err(_)) => Err(self.exit_reason("server exited")),
                    Err(_) => {
                        c.pending.lock().remove(&id);
                        Err(format!("{method} timed out after {}s", timeout.as_secs()))
                    }
                }
            }
            Conn::Http(c) => {
                let id = c.next.fetch_add(1, Ordering::Relaxed);
                let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
                let resp = tokio::time::timeout(timeout, c.post(body))
                    .await
                    .map_err(|_| format!("{method} timed out after {}s", timeout.as_secs()))??;
                let v = resp.ok_or("empty response")?;
                match v.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("server error").to_string()),
                    None => Ok(v["result"].clone()),
                }
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        let body = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &self.conn {
            Conn::Stdio(c) => {
                let mut w = c.stdin.lock().await;
                let _ = w.write_all(format!("{body}\n").as_bytes()).await;
                let _ = w.flush().await;
            }
            Conn::Http(c) => {
                let _ = c.post(body).await;
            }
        }
    }

    fn exit_reason(&self, base: &str) -> String {
        if let Conn::Stdio(c) = &self.conn {
            let tail: Vec<String> = c.stderr_tail.lock().iter().cloned().collect();
            if !tail.is_empty() {
                return format!("{base}: {}", tail.join(" | "));
            }
        }
        base.to_string()
    }

    /// All tools the server offers.
    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let r = self.request("tools/list", params, INIT_TIMEOUT).await?;
            if let Some(arr) = r["tools"].as_array() {
                tools.extend(arr.iter().cloned());
            }
            match r["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(tools)
    }
}

impl HttpClient {
    /// POST one JSON-RPC message; returns the matching response (None for 202).
    async fn post(&self, body: Value) -> Result<Option<Value>, String> {
        let want = body.get("id").cloned();
        let mut req = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(s) = self.session.lock().clone() {
            req = req.header("Mcp-Session-Id", s);
        }
        let resp = req
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|h| h.to_str().ok())
        {
            *self.session.lock() = Some(s.to_string());
        }
        let status = resp.status();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
        if !status.is_success() {
            let snippet: String = text.chars().take(200).collect();
            return Err(format!("HTTP {status}: {snippet}"));
        }
        if want.is_none() || text.trim().is_empty() {
            return Ok(None);
        }
        if ctype.contains("event-stream") {
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data:")
                    && let Ok(v) = serde_json::from_str::<Value>(data.trim())
                    && v.get("id") == want.as_ref()
                {
                    return Ok(Some(v));
                }
            }
            return Err("no response in event stream".into());
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("bad JSON: {e}"))
    }
}

// ----- tools -----------------------------------------------------------------------------

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn tool_prefix(server: &str) -> String {
    format!("mcp__{}__", sanitize(server))
}

struct McpTool {
    fq: String,
    remote: String,
    desc: String,
    schema: Value,
    client: Arc<Client>,
}

fn render_result(r: &Value) -> (String, bool) {
    let mut parts: Vec<String> = vec![];
    for b in r["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => parts.push(b["text"].as_str().unwrap_or("").to_string()),
            Some("image") => parts.push(format!(
                "[image: {}]",
                b["mimeType"].as_str().unwrap_or("unknown")
            )),
            Some("audio") => parts.push("[audio]".into()),
            Some("resource") => parts.push(
                b["resource"]["text"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "[resource {}]",
                            b["resource"]["uri"].as_str().unwrap_or("?")
                        )
                    }),
            ),
            Some("resource_link") => {
                parts.push(format!("[link {}]", b["uri"].as_str().unwrap_or("?")))
            }
            _ => {}
        }
    }
    let mut text = parts.join("\n");
    if text.is_empty()
        && let Some(s) = r.get("structuredContent").filter(|s| !s.is_null())
    {
        text = serde_json::to_string_pretty(s).unwrap_or_default();
    }
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… [output truncated]");
    }
    (text, r["isError"].as_bool() == Some(true))
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.fq
    }
    fn description(&self) -> &str {
        &self.desc
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let params = json!({ "name": self.remote, "arguments": inv.arguments });
            match self
                .client
                .request("tools/call", params, CALL_TIMEOUT)
                .await
            {
                Ok(r) => {
                    let (text, is_err) = render_result(&r);
                    if is_err {
                        ToolOutput::err(text)
                    } else {
                        ToolOutput::ok(if text.is_empty() {
                            "(no output)".into()
                        } else {
                            text
                        })
                    }
                }
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

// ----- the hub ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum McpState {
    /// Project-scope server waiting for the user's approval.
    Pending,
    Connecting,
    Ready,
    Failed(String),
    Disabled,
}

#[derive(Clone, Debug)]
pub struct McpStatus {
    pub name: String,
    pub scope: Scope,
    pub source: PathBuf,
    pub detail: String,
    pub state: McpState,
    pub tools: Vec<String>,
}

struct Entry {
    cfg: ServerConfig,
    state: McpState,
    tools: Vec<String>,
    client: Option<Arc<Client>>,
    /// Bumped on every (re)connect so a stale connect task cannot clobber a newer one.
    epoch: u64,
}

/// One workspace's MCP connections. Cheap to share; all work happens on tasks.
pub struct McpHub {
    cwd: PathBuf,
    registry: Arc<Registry>,
    handle: tokio::runtime::Handle,
    entries: Mutex<Vec<Entry>>,
}

impl McpHub {
    /// Load configs and start every user-scope or already-approved server.
    pub fn start(
        cwd: &Path,
        registry: Arc<Registry>,
        handle: tokio::runtime::Handle,
    ) -> Arc<McpHub> {
        let trust = trusted();
        let entries: Vec<Entry> = load_configs(cwd)
            .into_iter()
            .map(|cfg| {
                let state = if cfg.disabled {
                    McpState::Disabled
                } else if cfg.scope == Scope::Project && !trust.contains(&cfg.fingerprint()) {
                    McpState::Pending
                } else {
                    McpState::Connecting
                };
                Entry {
                    cfg,
                    state,
                    tools: vec![],
                    client: None,
                    epoch: 0,
                }
            })
            .collect();
        let hub = Arc::new(McpHub {
            cwd: cwd.to_path_buf(),
            registry,
            handle,
            entries: Mutex::new(entries),
        });
        let names: Vec<String> = hub
            .entries
            .lock()
            .iter()
            .filter(|e| e.state == McpState::Connecting)
            .map(|e| e.cfg.name.clone())
            .collect();
        for n in names {
            hub.connect(&n);
        }
        hub
    }

    pub fn statuses(&self) -> Vec<McpStatus> {
        self.entries
            .lock()
            .iter()
            .map(|e| McpStatus {
                name: e.cfg.name.clone(),
                scope: e.cfg.scope,
                source: e.cfg.source.clone(),
                detail: e.cfg.detail(),
                state: e.state.clone(),
                tools: e.tools.clone(),
            })
            .collect()
    }

    /// Total tools currently registered from MCP servers.
    pub fn tool_count(&self) -> usize {
        self.entries.lock().iter().map(|e| e.tools.len()).sum()
    }

    /// Trust a pending project server and connect it.
    pub fn approve(self: &Arc<Self>, name: &str) {
        let fp = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.cfg.name == name)
                .map(|e| e.cfg.fingerprint())
        };
        if let Some(fp) = fp {
            remember_trust(&fp);
            self.connect(name);
        }
    }

    pub fn reconnect(self: &Arc<Self>, name: &str) {
        self.connect(name);
    }

    pub fn set_enabled(self: &Arc<Self>, name: &str, enabled: bool) {
        if enabled {
            self.approve(name);
            return;
        }
        let mut entries = self.entries.lock();
        if let Some(e) = entries.iter_mut().find(|e| e.cfg.name == name) {
            e.epoch += 1;
            e.client = None; // drops the child process
            e.tools.clear();
            e.state = McpState::Disabled;
            self.registry.remove_prefix(&tool_prefix(name));
        }
    }

    fn connect(self: &Arc<Self>, name: &str) {
        let (cfg, epoch) = {
            let mut entries = self.entries.lock();
            let Some(e) = entries.iter_mut().find(|e| e.cfg.name == name) else {
                return;
            };
            e.epoch += 1;
            e.client = None;
            e.tools.clear();
            e.state = McpState::Connecting;
            self.registry.remove_prefix(&tool_prefix(name));
            (e.cfg.clone(), e.epoch)
        };
        let hub = self.clone();
        self.handle.spawn(async move {
            let outcome = async {
                let client = Arc::new(Client::connect(&cfg, &hub.cwd).await?);
                let raw = client.list_tools().await?;
                Ok::<_, String>((client, raw))
            }
            .await;
            let mut entries = hub.entries.lock();
            let Some(e) = entries.iter_mut().find(|e| e.cfg.name == cfg.name) else {
                return;
            };
            if e.epoch != epoch {
                return; // superseded by a newer connect or a disable
            }
            match outcome {
                Ok((client, raw)) => {
                    let prefix = tool_prefix(&cfg.name);
                    let mut names = vec![];
                    for t in raw {
                        let Some(remote) = t["name"].as_str() else {
                            continue;
                        };
                        let mut fq = format!("{prefix}{}", sanitize(remote));
                        if fq.len() > 64 {
                            fq = fq[..64].to_string();
                        }
                        let mut schema = t["inputSchema"].clone();
                        if !schema.is_object() {
                            schema = json!({"type":"object","properties":{}});
                        }
                        if schema.get("type").is_none() {
                            schema["type"] = json!("object");
                        }
                        let desc = format!(
                            "[{}] {}",
                            cfg.name,
                            t["description"].as_str().unwrap_or(remote)
                        );
                        hub.registry.register(Arc::new(McpTool {
                            fq: fq.clone(),
                            remote: remote.to_string(),
                            desc,
                            schema,
                            client: client.clone(),
                        }));
                        names.push(fq);
                    }
                    e.tools = names;
                    e.client = Some(client);
                    e.state = McpState::Ready;
                }
                Err(msg) => e.state = McpState::Failed(msg),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdio_and_http_servers() {
        let cfgs = parse_servers(
            r#"{"mcpServers":{
                "a":{"command":"npx","args":["-y","pkg"],"env":{"K":"v"}},
                "b":{"url":"https://x/mcp","headers":{"A":"1"}},
                "c":{"command":"x","disabled":true},
                "bad":{}
            }}"#,
            Path::new("mcp.json"),
            Scope::User,
        );
        assert_eq!(cfgs.len(), 3);
        assert!(
            matches!(&cfgs[0].transport, Transport::Stdio { command, args, .. } if command == "npx" && args.len() == 2)
        );
        assert!(
            matches!(&cfgs[1].transport, Transport::Http { url, .. } if url == "https://x/mcp")
        );
        assert!(cfgs[2].disabled);
        assert_eq!(cfgs[0].detail(), "npx -y pkg");
    }

    #[test]
    fn env_expansion_uses_defaults() {
        assert_eq!(
            expand_env("a-${DESTRO_SURELY_UNSET:-fallback}-b"),
            "a-fallback-b"
        );
        assert_eq!(expand_env("no vars"), "no vars");
        assert_eq!(expand_env("${unterminated"), "${unterminated");
    }

    #[test]
    fn tool_names_are_sanitised() {
        assert_eq!(tool_prefix("my server!"), "mcp__my_server___");
    }

    #[test]
    fn renders_mixed_content() {
        let (t, e) = render_result(&json!({
            "content":[{"type":"text","text":"hi"},{"type":"image","mimeType":"image/png"}],
            "isError": true
        }));
        assert_eq!(t, "hi\n[image: image/png]");
        assert!(e);
    }
}
