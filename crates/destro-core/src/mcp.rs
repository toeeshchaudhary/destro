//! Model Context Protocol client: connect to MCP servers and expose what they offer.
//!
//! Servers are configured in `mcp.json` files using the same shape other agent
//! tools use, so an existing `.mcp.json` works as is:
//!
//! ```json
//! { "mcpServers": {
//!     "files":  { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
//!     "remote": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${TOKEN}" } },
//!     "legacy": { "type": "sse", "url": "https://example.com/sse" }
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
//! Transports: stdio (newline-delimited JSON-RPC), streamable HTTP, and the
//! older HTTP+SSE. What is used from a server:
//!
//! * **tools** → registered as `mcp__<server>__<tool>`, refreshed live when the
//!   server sends `notifications/tools/list_changed`;
//! * **resources** → `mcp__<server>__list_resources` / `__read_resource` tools;
//! * **prompts** → [`McpHub::prompts`] and [`McpHub::get_prompt`] (the TUI offers
//!   them as slash commands).
//!
//! Remote servers that need OAuth are not supported: put a token in `headers`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use destro_tools::{BoxFuture, Registry, Tool, ToolContext, ToolInvocation, ToolOutput};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

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
    /// Streamable HTTP (one POST per message).
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
    /// The older protocol: a GET event stream plus a POST endpoint it announces.
    Sse {
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
            Transport::Sse { url, .. } => format!("{url} (sse)"),
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
            let (url, headers) = (expand_env(url), pairs("headers"));
            if v["type"].as_str() == Some("sse") {
                Transport::Sse { url, headers }
            } else {
                Transport::Http { url, headers }
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

// ----- JSON-RPC plumbing -----------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// What a server told us changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Tools,
    Prompts,
}

type ChangeTx = mpsc::UnboundedSender<Change>;

/// Complete a pending request from a response message. False when `v` is not one.
fn dispatch_response(pending: &Pending, v: &Value) -> bool {
    if v.get("method").is_some() {
        return false;
    }
    let Some(n) = v.get("id").and_then(Value::as_u64) else {
        return false;
    };
    if let Some(tx) = pending.lock().remove(&n) {
        let _ = tx.send(match v.get("error") {
            Some(e) => Err(e["message"].as_str().unwrap_or("server error").to_string()),
            None => Ok(v["result"].clone()),
        });
    }
    true
}

fn dispatch_notification(v: &Value, tx: &ChangeTx) {
    match v["method"].as_str() {
        Some("notifications/tools/list_changed") => {
            let _ = tx.send(Change::Tools);
        }
        Some("notifications/prompts/list_changed") => {
            let _ = tx.send(Change::Prompts);
        }
        _ => {}
    }
}

fn fail_all(pending: &Pending, why: &str) {
    for (_, tx) in pending.lock().drain() {
        let _ = tx.send(Err(why.to_string()));
    }
}

/// HTTP statuses that mean "log in": we cannot, so say what to do instead.
fn auth_error(status: u16) -> Option<String> {
    matches!(status, 401 | 403).then(|| {
        format!(
            "server requires authentication (HTTP {status}); OAuth is not supported yet — put a token in the server's `headers`"
        )
    })
}

/// Incremental parser for SSE with event names (the shared [`crate::provider::SseParser`]
/// drops them, and MCP's `endpoint` event is identified by name).
#[derive(Default)]
struct NamedSse {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl NamedSse {
    /// Feed bytes; returns completed `(event, data)` pairs.
    fn push(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(bytes);
        let mut out = vec![];
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push((std::mem::take(&mut self.event), self.data.join("\n")));
                    self.data.clear();
                } else {
                    self.event.clear();
                }
            } else if let Some(v) = line.strip_prefix("event:") {
                self.event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                self.data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
            }
            // comments (`:`) and other fields are ignored
        }
        out
    }
}

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

struct SseClient {
    http: reqwest::Client,
    post_url: String,
    headers: Vec<(String, String)>,
    pending: Pending,
    next: AtomicU64,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SseClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum Conn {
    Stdio(Box<StdioClient>),
    Http(Box<HttpClient>),
    Sse(Box<SseClient>),
}

/// A live connection to one server.
pub struct Client {
    conn: Conn,
    /// The `capabilities` object from the server's `initialize` result.
    caps: Value,
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
    async fn connect(cfg: &ServerConfig, cwd: &Path, changes: ChangeTx) -> Result<Client, String> {
        let conn = match &cfg.transport {
            Transport::Stdio {
                command,
                args,
                env,
                cwd: dir,
            } => Conn::Stdio(Box::new(spawn_stdio(
                command, args, env, dir, cwd, changes,
            )?)),
            Transport::Http { url, headers } => Conn::Http(Box::new(HttpClient {
                http: reqwest::Client::new(),
                url: url.clone(),
                headers: headers.clone(),
                session: Mutex::new(None),
                next: AtomicU64::new(1),
            })),
            Transport::Sse { url, headers } => {
                Conn::Sse(Box::new(connect_sse(url, headers, changes).await?))
            }
        };
        let mut client = Client {
            conn,
            caps: Value::Null,
        };
        let init = client
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
        client.caps = init["capabilities"].clone();
        client.notify("notifications/initialized", json!({})).await;
        Ok(client)
    }

    fn has(&self, capability: &str) -> bool {
        self.caps.get(capability).is_some_and(|v| !v.is_null())
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
            Conn::Sse(c) => {
                let id = c.next.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = oneshot::channel();
                c.pending.lock().insert(id, tx);
                let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
                if let Err(e) = c.post(body).await {
                    c.pending.lock().remove(&id);
                    return Err(e);
                }
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) => Err("event stream closed".into()),
                    Err(_) => {
                        c.pending.lock().remove(&id);
                        Err(format!("{method} timed out after {}s", timeout.as_secs()))
                    }
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
            Conn::Sse(c) => {
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

    /// Every item of a paginated list method, e.g. `tools/list` → `tools`.
    async fn list_all(&self, method: &str, key: &str) -> Result<Vec<Value>, String> {
        let mut items = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let r = self.request(method, params, INIT_TIMEOUT).await?;
            if let Some(arr) = r[key].as_array() {
                items.extend(arr.iter().cloned());
            }
            match r["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(items)
    }

    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        self.list_all("tools/list", "tools").await
    }
}

fn spawn_stdio(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    dir: &Option<PathBuf>,
    cwd: &Path,
    changes: ChangeTx,
) -> Result<StdioClient, String> {
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
                if dispatch_response(&pending, &v) {
                    continue;
                }
                let id = v.get("id").cloned().filter(|i| !i.is_null());
                match id {
                    // a request from the server: answer ping, refuse the rest
                    Some(id) => {
                        let reply = if v["method"] == "ping" {
                            json!({"jsonrpc":"2.0","id":id,"result":{}})
                        } else {
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not supported"}})
                        };
                        let mut w = stdin.lock().await;
                        let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
                        let _ = w.flush().await;
                    }
                    None => dispatch_notification(&v, &changes),
                }
            }
            fail_all(&pending, "server exited");
        });
    }
    Ok(StdioClient {
        stdin,
        pending,
        next: AtomicU64::new(1),
        stderr_tail: tail,
        _child: child,
    })
}

async fn connect_sse(
    url: &str,
    headers: &[(String, String)],
    changes: ChangeTx,
) -> Result<SseClient, String> {
    let http = reqwest::Client::new();
    let mut req = http.get(url).header("Accept", "text/event-stream");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("cannot open event stream: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(auth_error(status.as_u16()).unwrap_or_else(|| format!("HTTP {status}")));
    }
    let base = resp.url().clone();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let (etx, erx) = oneshot::channel::<String>();
    let task = {
        let pending = pending.clone();
        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut parser = NamedSse::default();
            let mut endpoint = Some(etx);
            while let Some(Ok(chunk)) = stream.next().await {
                for (event, data) in parser.push(&chunk) {
                    match event.as_str() {
                        "endpoint" => {
                            if let Some(t) = endpoint.take() {
                                let _ = t.send(data);
                            }
                        }
                        "" | "message" => {
                            if let Ok(v) = serde_json::from_str::<Value>(&data)
                                && !dispatch_response(&pending, &v)
                            {
                                dispatch_notification(&v, &changes);
                            }
                        }
                        _ => {}
                    }
                }
            }
            fail_all(&pending, "event stream closed");
        })
    };
    let endpoint = match tokio::time::timeout(Duration::from_secs(15), erx).await {
        Ok(Ok(e)) => e,
        _ => {
            task.abort();
            return Err("server never announced a message endpoint".into());
        }
    };
    let post_url = base
        .join(&endpoint)
        .map_err(|e| format!("bad endpoint {endpoint:?}: {e}"))?
        .to_string();
    Ok(SseClient {
        http,
        post_url,
        headers: headers.to_vec(),
        pending,
        next: AtomicU64::new(1),
        task,
    })
}

impl SseClient {
    /// POST one message; the answer arrives on the event stream.
    async fn post(&self, body: Value) -> Result<(), String> {
        let mut req = self
            .http
            .post(&self.post_url)
            .header("Content-Type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(auth_error(status.as_u16()).unwrap_or_else(|| format!("HTTP {status}")))
        }
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
            if let Some(m) = auth_error(status.as_u16()) {
                return Err(m);
            }
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

fn clip(mut text: String) -> String {
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… [output truncated]");
    }
    text
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
    (clip(text), r["isError"].as_bool() == Some(true))
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

/// `list_resources` / `read_resource` for a server that offers resources.
struct ResourceTool {
    fq: String,
    server: String,
    read: bool,
    client: Arc<Client>,
}

fn render_resource_list(items: &[Value], templates: bool) -> String {
    items
        .iter()
        .map(|r| {
            let id = r["uri"]
                .as_str()
                .or(r["uriTemplate"].as_str())
                .unwrap_or("?");
            let mut line = format!("{id} — {}", r["name"].as_str().unwrap_or(""));
            if let Some(m) = r["mimeType"].as_str() {
                line.push_str(&format!(" ({m})"));
            }
            if let Some(d) = r["description"].as_str() {
                line.push_str(&format!(": {d}"));
            }
            if templates {
                line.push_str("  [template]");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_resource_contents(r: &Value) -> String {
    r["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| match c["text"].as_str() {
            Some(t) => t.to_string(),
            None => format!(
                "[binary {}, {} bytes]",
                c["mimeType"].as_str().unwrap_or("data"),
                c["blob"].as_str().map(|b| b.len() * 3 / 4).unwrap_or(0)
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tool for ResourceTool {
    fn name(&self) -> &str {
        &self.fq
    }
    fn description(&self) -> &str {
        if self.read {
            "Read one resource of this MCP server by URI (see list_resources)."
        } else {
            "List the resources (files, records, documents) this MCP server exposes."
        }
    }
    fn schema(&self) -> Value {
        if self.read {
            json!({"type":"object","properties":{"uri":{"type":"string","description":"Resource URI."}},"required":["uri"]})
        } else {
            json!({"type":"object","properties":{}})
        }
    }
    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            if self.read {
                let Some(uri) = inv.arguments["uri"].as_str() else {
                    return ToolOutput::err("missing required argument `uri`");
                };
                match self
                    .client
                    .request("resources/read", json!({ "uri": uri }), CALL_TIMEOUT)
                    .await
                {
                    Ok(r) => ToolOutput::ok(clip(render_resource_contents(&r))),
                    Err(e) => ToolOutput::err(e),
                }
            } else {
                let items = match self.client.list_all("resources/list", "resources").await {
                    Ok(i) => i,
                    Err(e) => return ToolOutput::err(format!("[{}] {e}", self.server)),
                };
                let mut out = render_resource_list(&items, false);
                if let Ok(t) = self
                    .client
                    .list_all("resources/templates/list", "resourceTemplates")
                    .await
                    && !t.is_empty()
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&render_resource_list(&t, true));
                }
                ToolOutput::ok(if out.is_empty() {
                    "no resources".into()
                } else {
                    clip(out)
                })
            }
        })
    }
}

// ----- prompts ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpPromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// A prompt template a server offers (the TUI lists these as commands).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
}

fn parse_prompts(server: &str, raw: &[Value]) -> Vec<McpPrompt> {
    raw.iter()
        .filter_map(|p| {
            Some(McpPrompt {
                server: server.to_string(),
                name: p["name"].as_str()?.to_string(),
                description: p["description"].as_str().unwrap_or("").to_string(),
                arguments: p["arguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| {
                        Some(McpPromptArg {
                            name: a["name"].as_str()?.to_string(),
                            description: a["description"].as_str().unwrap_or("").to_string(),
                            required: a["required"].as_bool() == Some(true),
                        })
                    })
                    .collect(),
            })
        })
        .collect()
}

/// `prompts/get` messages as plain text: `role: text` blocks.
fn render_prompt(r: &Value) -> String {
    r["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let blocks: Vec<&Value> = match &m["content"] {
                Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            let text = blocks
                .iter()
                .map(|b| match b["type"].as_str() {
                    Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                    Some("image") => "[image]".to_string(),
                    Some("resource") => b["resource"]["text"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| "[resource]".to_string()),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("{}: {text}", m["role"].as_str().unwrap_or("user"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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
    pub has_resources: bool,
    pub prompts: usize,
}

struct Entry {
    cfg: ServerConfig,
    state: McpState,
    tools: Vec<String>,
    prompts: Vec<McpPrompt>,
    has_resources: bool,
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
                    prompts: vec![],
                    has_resources: false,
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
                has_resources: e.has_resources,
                prompts: e.prompts.len(),
            })
            .collect()
    }

    /// Total tools currently registered from MCP servers.
    pub fn tool_count(&self) -> usize {
        self.entries.lock().iter().map(|e| e.tools.len()).sum()
    }

    /// Prompts of every connected server.
    pub fn prompts(&self) -> Vec<McpPrompt> {
        self.entries
            .lock()
            .iter()
            .filter(|e| e.state == McpState::Ready)
            .flat_map(|e| e.prompts.clone())
            .collect()
    }

    /// Render one prompt with the given arguments as plain text.
    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        args: serde_json::Map<String, Value>,
    ) -> Result<String, String> {
        let client = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.cfg.name == server && e.state == McpState::Ready)
                .and_then(|e| e.client.clone())
                .ok_or_else(|| format!("server `{server}` is not connected"))?
        };
        let r = client
            .request(
                "prompts/get",
                json!({ "name": name, "arguments": args }),
                CALL_TIMEOUT,
            )
            .await?;
        Ok(render_prompt(&r))
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
            e.prompts.clear();
            e.has_resources = false;
            e.state = McpState::Disabled;
            self.registry.remove_prefix(&tool_prefix(name));
        }
    }

    /// Replace a server's registered tools with `raw` (plus resource tools).
    fn install_tools(&self, e: &mut Entry, client: &Arc<Client>, raw: Vec<Value>) {
        let server = e.cfg.name.clone();
        let prefix = tool_prefix(&server);
        self.registry.remove_prefix(&prefix);
        let mut names = vec![];
        for t in raw {
            let Some(remote) = t["name"].as_str() else {
                continue;
            };
            let mut fq = format!("{prefix}{}", sanitize(remote));
            if fq.len() > 64 {
                fq.truncate(64);
            }
            let mut schema = t["inputSchema"].clone();
            if !schema.is_object() {
                schema = json!({"type":"object","properties":{}});
            }
            if schema.get("type").is_none() {
                schema["type"] = json!("object");
            }
            let desc = format!("[{server}] {}", t["description"].as_str().unwrap_or(remote));
            self.registry.register(Arc::new(McpTool {
                fq: fq.clone(),
                remote: remote.to_string(),
                desc,
                schema,
                client: client.clone(),
            }));
            names.push(fq);
        }
        e.has_resources = client.has("resources");
        if e.has_resources {
            for (suffix, read) in [("list_resources", false), ("read_resource", true)] {
                let fq = format!("{prefix}{suffix}");
                self.registry.register(Arc::new(ResourceTool {
                    fq: fq.clone(),
                    server: server.clone(),
                    read,
                    client: client.clone(),
                }));
                names.push(fq);
            }
        }
        e.tools = names;
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
            e.prompts.clear();
            e.has_resources = false;
            e.state = McpState::Connecting;
            self.registry.remove_prefix(&tool_prefix(name));
            (e.cfg.clone(), e.epoch)
        };
        let hub = self.clone();
        self.handle.spawn(async move {
            let (ctx, mut crx) = mpsc::unbounded_channel::<Change>();
            let outcome = async {
                let client = Arc::new(Client::connect(&cfg, &hub.cwd, ctx).await?);
                let raw = client.list_tools().await?;
                let prompts = if client.has("prompts") {
                    client
                        .list_all("prompts/list", "prompts")
                        .await
                        .map(|p| parse_prompts(&cfg.name, &p))
                        .unwrap_or_default()
                } else {
                    vec![]
                };
                Ok::<_, String>((client, raw, prompts))
            }
            .await;
            let client = {
                let mut entries = hub.entries.lock();
                let Some(e) = entries.iter_mut().find(|e| e.cfg.name == cfg.name) else {
                    return;
                };
                if e.epoch != epoch {
                    return; // superseded by a newer connect or a disable
                }
                match outcome {
                    Ok((client, raw, prompts)) => {
                        hub.install_tools(e, &client, raw);
                        e.prompts = prompts;
                        e.client = Some(client.clone());
                        e.state = McpState::Ready;
                        client
                    }
                    Err(msg) => {
                        e.state = McpState::Failed(msg);
                        return;
                    }
                }
            };
            // live refresh: the server tells us when its lists change
            while let Some(first) = crx.recv().await {
                let mut tools = first == Change::Tools;
                let mut prompts = first == Change::Prompts;
                while let Ok(more) = crx.try_recv() {
                    tools |= more == Change::Tools;
                    prompts |= more == Change::Prompts;
                }
                if tools && let Ok(raw) = client.list_tools().await {
                    let mut entries = hub.entries.lock();
                    match entries.iter_mut().find(|e| e.cfg.name == cfg.name) {
                        Some(e) if e.epoch == epoch => hub.install_tools(e, &client, raw),
                        _ => return,
                    }
                }
                if prompts && let Ok(p) = client.list_all("prompts/list", "prompts").await {
                    let mut entries = hub.entries.lock();
                    match entries.iter_mut().find(|e| e.cfg.name == cfg.name) {
                        Some(e) if e.epoch == epoch => e.prompts = parse_prompts(&cfg.name, &p),
                        _ => return,
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdio_http_and_sse_servers() {
        let cfgs = parse_servers(
            r#"{"mcpServers":{
                "a":{"command":"npx","args":["-y","pkg"],"env":{"K":"v"}},
                "b":{"url":"https://x/mcp","headers":{"A":"1"}},
                "c":{"command":"x","disabled":true},
                "d":{"type":"sse","url":"https://x/sse"},
                "bad":{}
            }}"#,
            Path::new("mcp.json"),
            Scope::User,
        );
        assert_eq!(cfgs.len(), 4);
        assert!(
            matches!(&cfgs[0].transport, Transport::Stdio { command, args, .. } if command == "npx" && args.len() == 2)
        );
        assert!(
            matches!(&cfgs[1].transport, Transport::Http { url, .. } if url == "https://x/mcp")
        );
        assert!(cfgs[2].disabled);
        assert!(matches!(&cfgs[3].transport, Transport::Sse { url, .. } if url == "https://x/sse"));
        assert_eq!(cfgs[0].detail(), "npx -y pkg");
        assert_eq!(cfgs[3].detail(), "https://x/sse (sse)");
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

    #[test]
    fn named_sse_handles_split_chunks_crlf_and_comments() {
        let mut p = NamedSse::default();
        assert!(p.push(b"event: endpoi").is_empty());
        let out = p.push(b"nt\r\ndata: /messages?s=1\r\n\r\n: keepalive\r\nevent: message\ndata: {\"a\":\ndata: 1}\n\n");
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            ("endpoint".to_string(), "/messages?s=1".to_string())
        );
        assert_eq!(out[1], ("message".to_string(), "{\"a\":\n1}".to_string()));
        // a data-only event has an empty name, and names do not leak between events
        let out = p.push(b"data: x\n\n");
        assert_eq!(out, vec![(String::new(), "x".to_string())]);
    }

    #[test]
    fn auth_failures_say_what_to_do() {
        assert!(auth_error(401).unwrap().contains("`headers`"));
        assert!(auth_error(403).is_some());
        assert!(auth_error(500).is_none());
    }

    #[test]
    fn prompts_parse_and_render() {
        let p = parse_prompts(
            "s",
            &[
                json!({"name":"review","description":"Review code","arguments":[
                {"name":"file","description":"path","required":true},{"name":"style"}]}),
            ],
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].arguments.len(), 2);
        assert!(p[0].arguments[0].required && !p[0].arguments[1].required);
        let text = render_prompt(&json!({"messages":[
            {"role":"user","content":{"type":"text","text":"Review a.rs"}},
            {"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"image"}]}]}));
        assert_eq!(text, "user: Review a.rs\n\nassistant: ok\n[image]");
    }

    #[test]
    fn resource_rendering() {
        let l = render_resource_list(
            &[json!({"uri":"file:///a","name":"a","mimeType":"text/plain","description":"d"})],
            false,
        );
        assert_eq!(l, "file:///a — a (text/plain): d");
        let c = render_resource_contents(&json!({"contents":[
            {"uri":"x","text":"hello"},{"uri":"y","mimeType":"image/png","blob":"AAAA"}]}));
        assert_eq!(c, "hello\n[binary image/png, 3 bytes]");
    }
}
