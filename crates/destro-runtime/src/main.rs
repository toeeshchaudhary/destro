//! `destro-harness`: the runtime side of Destro, speaking the dsh SDK protocol.
//!
//! One process serves one workspace. A client (the TUI, the Python SDK, a script)
//! sends newline-delimited JSON-RPC on stdin and gets notifications on stdout:
//! exactly what `dsh --profile sdk` does, plus the `agent/*` control methods that
//! let a client inspect and steer individual agents.
//!
//! Phase 0 uses the deterministic mock provider so the whole surface is runnable
//! with no API key. The provider is a single construction site; swapping in the
//! DeepSeek adapter does not change anything below.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use destro_core::{AgentId, Delivery, Harness, HarnessConfig, HarnessEvent, SpawnSpec};
use destro_proto::{
    AgentState, Frame, InitializeParams, InitializeResult, SessionQueryParams, SpawnParams,
    SubagentFinishedNotification, SubagentStartedNotification, ext, method, notify,
};

const USAGE: &str = "destro-harness — Rust harness speaking the dsh SDK protocol over stdio

USAGE
  destro-harness [options]

OPTIONS
  --cwd <dir>          workspace directory (default: current directory)
  --model <id>         model route (default: the provider's default)
  --concurrency <n>    concurrent provider requests (default: 24)
  --provider <id>      provider route (default: first configured provider, else mock)
  --list-providers     print known providers and exit
  -h, --help           show this help

PROVIDERS
  Built in: mock, deepseek, openai-compatible. Add your own in
  %APPDATA%\\destro\\providers.json — see docs/PROVIDERS.md.
";

struct Args {
    cwd: PathBuf,
    model: Option<String>,
    concurrency: usize,
    provider: Option<String>,
    list_providers: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        cwd: std::env::current_dir()?,
        model: None,
        concurrency: 24,
        provider: None,
        list_providers: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--cwd" => args.cwd = PathBuf::from(it.next().unwrap_or_default()),
            "--model" => args.model = it.next(),
            "--concurrency" => {
                args.concurrency = it.next().and_then(|n| n.parse().ok()).unwrap_or(24)
            }
            "--provider" => args.provider = it.next(),
            "--list-providers" => args.list_providers = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(args)
}

type Out = mpsc::UnboundedSender<String>;

#[derive(Default)]
struct Sessions {
    by_session: HashMap<String, AgentId>,
    by_agent: HashMap<AgentId, String>,
}

impl Sessions {
    fn bind(&mut self, session: &str, agent: AgentId) {
        self.by_session.insert(session.to_string(), agent);
        self.by_agent.insert(agent, session.to_string());
    }

    fn agent(&self, session: &str) -> Option<AgentId> {
        self.by_session.get(session).copied()
    }

    /// The client's session id for an agent, falling back to the runtime's own.
    fn session_of(&self, agent: AgentId) -> String {
        self.by_agent
            .get(&agent)
            .cloned()
            .unwrap_or_else(|| agent.session().to_string())
    }
}

struct Server {
    harness: Arc<Harness>,
    sessions: Mutex<Sessions>,
    cwd: Mutex<PathBuf>,
    out: Out,
}

impl Server {
    fn send(&self, frame: Frame) {
        let _ = self.out.send(frame.to_line());
    }

    /// Bind a client session id to an agent, creating a root agent on demand.
    fn agent_for(&self, session: &str) -> Result<AgentId, String> {
        if let Some(id) = self.sessions.lock().unwrap().agent(session) {
            return Ok(id);
        }
        let cwd = self.cwd.lock().unwrap().clone();
        let handle = self.harness.spawn(
            None,
            SpawnSpec {
                title: Some(session.to_string()),
                cwd: Some(cwd),
                ..SpawnSpec::default()
            },
        )?;
        self.sessions.lock().unwrap().bind(session, handle.id);
        Ok(handle.id)
    }

    fn handle(&self, agent: AgentId) -> Result<destro_core::AgentHandle, String> {
        self.harness
            .get(agent)
            .ok_or_else(|| format!("unknown agent {agent}"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let registry = destro_core::ProviderRegistry::load();

    if args.list_providers {
        for info in registry.list() {
            println!(
                "{:<22} {:<10} {}{}",
                info.id,
                if info.configured {
                    "configured"
                } else {
                    "needs-key"
                },
                info.default_model,
                if info.models.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", info.models.join(", "))
                }
            );
        }
        return Ok(());
    }

    let provider_id = args.provider.unwrap_or_else(|| registry.default_id());
    let mut providers: std::collections::HashMap<String, Arc<dyn destro_core::Provider>> =
        std::collections::HashMap::new();
    for info in registry.list() {
        if info.configured
            && let Ok(provider) = registry.build(&info.id)
        {
            providers.insert(info.id, provider);
        }
    }
    if !providers.contains_key(&provider_id) {
        let provider = registry
            .build(&provider_id)
            .map_err(|e| anyhow::anyhow!("provider {provider_id}: {e}"))?;
        providers.insert(provider_id.clone(), provider);
    }
    let routes = destro_core::ProviderSet::new(provider_id.clone(), providers);
    let default_model = registry
        .descriptors()
        .iter()
        .find(|d| d.id == provider_id)
        .map(|d| d.default_model.clone())
        .unwrap_or_else(|| "deepseek-v4-pro".into());

    let mut cfg = HarnessConfig::new(routes.default_provider());
    cfg.providers = Some(routes);
    cfg.model = args.model.unwrap_or(default_model);
    cfg.cwd = args.cwd.clone();
    cfg.concurrency = args.concurrency;
    cfg.system = "You are Destro, a coding and general-purpose agent.".into();
    let harness = Harness::new(cfg);

    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
            {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let server = Arc::new(Server {
        harness: harness.clone(),
        sessions: Mutex::new(Sessions::default()),
        cwd: Mutex::new(args.cwd),
        out,
    });

    tokio::spawn(pump_events(harness.subscribe(), server.clone()));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut in_flight: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    while let Some(line) = lines.next_line().await? {
        let Some(frame) = Frame::parse(&line) else {
            continue;
        };
        if let Frame::Request { id, method, params } = frame {
            let server = server.clone();
            in_flight.push(tokio::spawn(async move {
                if let Some(response) = dispatch(&server, &method, params).await {
                    server.send(Frame::Response {
                        id,
                        result: response,
                    });
                }
            }));
        }
    }
    // Requests may still be spawning agents; letting them race the shutdown
    // snapshot would leave an unkilled agent holding the event bus open.
    for task in in_flight {
        let _ = task.await;
    }
    trace("requests settled");

    harness.shutdown().await;
    trace("harness shut down");
    // Drain deterministically: the event pump ends when the harness bus closes,
    // and the writer ends when the last sender is gone. The timeout is a safety
    // net: a stuck observer must not keep the process alive forever.
    drop(server);
    drop(harness);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    trace("writer drained");
    Ok(())
}

fn trace(msg: &str) {
    if std::env::var_os("DESTRO_TRACE").is_some() {
        eprintln!("[destro-harness] {msg}");
    }
}

async fn dispatch(server: &Arc<Server>, method: &str, params: Value) -> Option<Value> {
    match method {
        method::INITIALIZE => {
            let init: InitializeParams = serde_json::from_value(params).unwrap_or_default();
            if let Some(cwd) = init.cwd {
                *server.cwd.lock().unwrap() = PathBuf::from(cwd);
            }
            Some(serde_json::to_value(InitializeResult::default()).unwrap_or_else(|_| json!({})))
        }
        method::SESSION_QUERY | method::SESSION_PROMPT => {
            let Ok(query) = serde_json::from_value::<SessionQueryParams>(params) else {
                return Some(json!({}));
            };
            let text = destro_proto::blocks_text(&query.content_blocks);
            let agent = match server.agent_for(&query.session_id) {
                Ok(a) => a,
                Err(e) => return Some(json!({ "kind": "error", "messageId": e })),
            };
            if !text.trim().is_empty()
                && let Ok(handle) = server.handle(agent)
            {
                let _ = handle.send(text, Delivery::Queue);
            }
            Some(json!({ "kind": "prompt", "messageId": format!("u-{}", agent.0) }))
        }
        method::SESSION_WAIT => {
            let session = params["sessionId"].as_str().unwrap_or("");
            let agent = server.sessions.lock().unwrap().agent(session);
            if let Some(agent) = agent {
                let _ = server.harness.wait(agent).await;
            }
            Some(json!({}))
        }
        method::SESSION_RESUME => Some(json!({})),
        method::SESSION_WD_GET => {
            let cwd = server.cwd.lock().unwrap().clone();
            Some(json!({ "cwd": cwd.to_string_lossy() }))
        }
        method::SESSION_WD_SET => {
            if let Some(cwd) = params["cwd"].as_str() {
                *server.cwd.lock().unwrap() = PathBuf::from(cwd);
            }
            let cwd = server.cwd.lock().unwrap().clone();
            Some(json!({ "cwd": cwd.to_string_lossy() }))
        }
        method::SHUTDOWN => {
            server.harness.shutdown().await;
            Some(json!({}))
        }
        ext::AGENT_LIST => {
            let mut agents: Vec<AgentState> = server.harness.list();
            if let Some(parent) = params["parent"].as_str().and_then(AgentId::parse) {
                let descendants = server.harness.descendants(parent);
                agents.retain(|s| {
                    AgentId::parse(&s.agent_id)
                        .map(|id| descendants.contains(&id))
                        .unwrap_or(false)
                });
            }
            if let Some(status) = params["status"].as_str() {
                agents.retain(|s| s.status == status);
            }
            Some(json!({ "agents": agents }))
        }
        ext::AGENT_STATE => {
            let agent = params["agentId"].as_str().and_then(AgentId::parse);
            match agent.and_then(|a| server.harness.state(a)) {
                Some(state) => Some(serde_json::to_value(state).unwrap_or_else(|_| json!({}))),
                None => Some(json!({ "error": "unknown agent" })),
            }
        }
        ext::AGENT_SPAWN => {
            let Ok(spawn) = serde_json::from_value::<SpawnParams>(params) else {
                return Some(json!({ "error": "invalid spawn params" }));
            };
            let parent = spawn.parent_id.as_deref().and_then(AgentId::parse);
            let spec = SpawnSpec {
                title: spawn.title,
                model: spawn.model,
                system: spawn.system,
                cwd: None,
                prompt: spawn.prompt,
                priority: None,
            };
            match server.harness.spawn(parent, spec) {
                Ok(handle) => Some(json!({
                    "agentId": handle.id.wire(),
                    "sessionId": handle.live.session_id.to_string(),
                })),
                Err(e) => Some(json!({ "error": e })),
            }
        }
        ext::AGENT_SEND => {
            let id = params["agentId"].as_str().and_then(AgentId::parse);
            let text = params["text"].as_str().unwrap_or("");
            let mode = Delivery::from_wire(params["mode"].as_str());
            match id.and_then(|a| server.harness.get(a).map(|h| (a, h))) {
                Some((_, handle)) => match handle.send(text, mode) {
                    Ok(()) => Some(json!({ "ok": true })),
                    Err(e) => Some(json!({ "error": e })),
                },
                None => Some(json!({ "error": "unknown agent" })),
            }
        }
        ext::AGENT_INTERRUPT => {
            let id = params["agentId"].as_str().and_then(AgentId::parse);
            match id.and_then(|a| server.harness.get(a)) {
                Some(handle) => {
                    handle.interrupt();
                    Some(json!({ "ok": true }))
                }
                None => Some(json!({ "error": "unknown agent" })),
            }
        }
        ext::AGENT_KILL => {
            let id = params["agentId"].as_str().and_then(AgentId::parse);
            match id.map(|a| server.harness.kill(a)) {
                Some(Ok(killed)) => Some(json!({ "killed": killed })),
                Some(Err(e)) => Some(json!({ "error": e })),
                None => Some(json!({ "error": "invalid agent id" })),
            }
        }
        ext::AGENT_WAIT => {
            let id = params["agentId"].as_str().and_then(AgentId::parse);
            match id {
                Some(a) => match server.harness.wait(a).await {
                    Ok(state) => Some(serde_json::to_value(state).unwrap_or_else(|_| json!({}))),
                    Err(e) => Some(json!({ "error": e })),
                },
                None => Some(json!({ "error": "invalid agent id" })),
            }
        }
        ext::AGENT_MODELS => Some(json!({
            "models": [{ "id": server.harness.model, "provider": server.harness.provider.name() }]
        })),
        ext::AGENT_TRANSCRIPT => {
            let id = params["agentId"].as_str().and_then(AgentId::parse);
            match id.and_then(|a| server.harness.state(a)) {
                Some(state) => {
                    let mut events = Vec::new();
                    if let Some(last) = state.last_message {
                        events.push(json!({
                            "type": "assistant/message",
                            "data": {
                                "message": { "role": "assistant", "content": [{ "type": "text", "text": last }] },
                                "interrupted": false
                            }
                        }));
                    }
                    Some(json!({ "events": events }))
                }
                None => Some(json!({ "error": "unknown agent" })),
            }
        }
        _ => Some(json!({ "error": format!("method not found: {method}") })),
    }
}

/// Translate harness events into the SDK notifications the client expects.
async fn pump_events(mut rx: broadcast::Receiver<HarnessEvent>, server: Arc<Server>) {
    loop {
        match rx.recv().await {
            Ok(HarnessEvent::Session {
                agent,
                session_id,
                event,
            }) => {
                let session = server.sessions.lock().unwrap().session_of(agent);
                let _ = session_id; // the client's id wins when one is bound
                server.send(Frame::notification(
                    notify::SESSION_EVENT,
                    json!({ "sessionId": session, "event": *event }),
                ));
            }
            Ok(HarnessEvent::Status { agent, status }) => {
                let session = server.sessions.lock().unwrap().session_of(agent);
                server.send(Frame::notification(
                    notify::SESSION_STATUS,
                    json!({ "sessionId": session, "status": status.as_str() }),
                ));
            }
            Ok(HarnessEvent::Spawned {
                agent,
                parent: Some(parent),
                title,
            }) => {
                let sessions = server.sessions.lock().unwrap();
                let parent_session = sessions.session_of(parent);
                let child_session = sessions.session_of(agent);
                drop(sessions);
                let note = SubagentStartedNotification {
                    parent_session_id: parent_session,
                    child_session_id: child_session,
                    title,
                };
                server.send(Frame::notification(
                    notify::SUBAGENT_STARTED,
                    serde_json::to_value(note).unwrap_or_else(|_| json!({})),
                ));
            }
            Ok(HarnessEvent::Finished {
                agent,
                status,
                last_message,
            }) => {
                let sessions = server.sessions.lock().unwrap();
                let child_session = sessions.session_of(agent);
                drop(sessions);
                let note = SubagentFinishedNotification {
                    child_session_id: child_session,
                    status: match status {
                        destro_core::AgentStatus::Failed | destro_core::AgentStatus::Killed => {
                            "failed".into()
                        }
                        _ => "ok".into(),
                    },
                    last_assistant_message: last_message,
                };
                server.send(Frame::notification(
                    notify::SUBAGENT_FINISHED,
                    serde_json::to_value(note).unwrap_or_else(|_| json!({})),
                ));
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
