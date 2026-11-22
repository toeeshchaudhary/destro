//! In-process backend: the TUI drives Destro's own Rust harness instead of
//! spawning `dsh` children.
//!
//! The TUI is synchronous; the harness is Tokio. One runtime lives inside this
//! backend, one harness per workspace, and a pump task translates harness events
//! into the same JSON-RPC notification lines the TUI already parses. The frame
//! path stays identical to the dsh backend, so nothing above `Backend` changes.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use destro_core::context;
use destro_core::interact::Decision;
use destro_core::mcp::{McpHub, McpStatus};
use destro_core::skills::{self, SkillTool};
use destro_core::store::SessionWriter;
use destro_core::{
    AgentId, Delivery, Harness, HarnessConfig, HarnessEvent, ProviderSet, SpawnSpec,
};
use destro_proto::{Frame, ServerInfo, ext, method, notify};

use crate::backend::{Backend, Msg, ProjectSpec};
use crate::model::Project;

type Sessions = Arc<Mutex<Map>>;

#[derive(Default)]
struct Map {
    by_session: HashMap<String, AgentId>,
    by_agent: HashMap<AgentId, String>,
}

impl Map {
    fn bind(&mut self, session: &str, agent: AgentId) {
        self.by_session.insert(session.to_string(), agent);
        self.by_agent.insert(agent, session.to_string());
    }

    fn agent(&self, session: &str) -> Option<AgentId> {
        self.by_session.get(session).copied()
    }

    fn session_of(&self, agent: AgentId) -> String {
        self.by_agent
            .get(&agent)
            .cloned()
            .unwrap_or_else(|| agent.session().to_string())
    }
}

struct Slot {
    harness: Arc<Harness>,
    sessions: Sessions,
    cwd: String,
    name: String,
    #[allow(dead_code)]
    setup: Setup,
    mcp: Arc<McpHub>,
    /// Model chosen with `/model`; new sessions start on it.
    model_override: Arc<Mutex<Option<String>>>,
}

/// The routes a slot's harness was built with; kept so advisor mode can rebuild.
#[derive(Clone)]
struct Setup {
    model: String,
    system: String,
    slave_model: Option<String>,
    slave_system: Option<String>,
}

fn normal_setup(model: &str) -> Setup {
    Setup {
        model: model.to_string(),
        system: "You are Destro, a coding and general-purpose agent.".into(),
        slave_model: None,
        slave_system: None,
    }
}

fn advisor_setup(advisor: &str, slave: &str) -> Setup {
    Setup {
        model: advisor.to_string(),
        system: destro_core::advisor_system(advisor, slave),
        slave_model: Some(slave.to_string()),
        slave_system: Some(destro_core::slave_system()),
    }
}

pub struct Local {
    rt: tokio::runtime::Runtime,
    slots: Mutex<Vec<Option<Slot>>>,
    tx: Sender<Msg>,
    model: String,
    providers: Arc<ProviderSet>,
    persist: bool,
    approvals: std::sync::atomic::AtomicBool,
}

impl Local {
    pub fn start(
        specs: &[ProjectSpec],
        model: &str,
        provider: Arc<ProviderSet>,
        tx: Sender<Msg>,
        persist: bool,
    ) -> (Vec<Project>, Local) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("destro-harness")
            .build()
            .expect("build tokio runtime");
        let mut slots = Vec::with_capacity(specs.len());
        for (i, spec) in specs.iter().enumerate() {
            slots.push(Some(Self::slot(
                &rt,
                i,
                &spec.name,
                &spec.cwd,
                normal_setup(model),
                provider.clone(),
                &tx,
                persist,
                false,
            )));
        }
        let backend = Local {
            rt,
            slots: Mutex::new(slots),
            tx,
            model: model.to_string(),
            providers: provider,
            persist,
            approvals: std::sync::atomic::AtomicBool::new(false),
        };
        let projects = specs
            .iter()
            .map(|s| Project::new(&s.name, &s.cwd, model))
            .collect();
        (projects, backend)
    }

    #[allow(clippy::too_many_arguments)]
    fn slot(
        rt: &tokio::runtime::Runtime,
        index: usize,
        name: &str,
        cwd: &str,
        setup: Setup,
        providers: Arc<ProviderSet>,
        tx: &Sender<Msg>,
        persist: bool,
        approvals: bool,
    ) -> Slot {
        let root = std::path::PathBuf::from(cwd);
        // Conventions: the AGENTS.md / CLAUDE.md chain and the skill catalogue go
        // into every agent's system prompt, planner and workers alike.
        let instr = context::load_instructions(&root);
        let found_skills = skills::discover_skills(&root);
        let mut cfg = HarnessConfig::new(providers.default_provider());
        cfg.providers = Some(providers);
        cfg.model = setup.model.clone();
        cfg.cwd = root.clone();
        cfg.system = context::assemble(&setup.system, &root, &instr, &found_skills);
        cfg.slave_model = setup.slave_model.clone();
        cfg.slave_system = setup
            .slave_system
            .as_ref()
            .map(|s| context::assemble(s, &root, &instr, &found_skills));
        let harness = Harness::new(cfg);
        harness.interact.attach();
        harness.interact.set_approvals(approvals);
        let advisor = setup
            .slave_model
            .clone()
            .map(|slave| (setup.model.clone(), slave));
        if !found_skills.is_empty() {
            harness
                .tools
                .register(Arc::new(SkillTool::new(found_skills)));
        }
        let mcp = McpHub::start(&root, harness.tools.clone(), rt.handle().clone());
        let writers: Arc<Mutex<HashMap<String, SessionWriter>>> = Arc::default();
        let pump_harness = harness.clone();
        let pump_cwd = cwd.to_string();
        let sessions: Sessions = Arc::new(Mutex::new(Map::default()));
        let mut rx = harness.subscribe();
        let out = tx.clone();
        let map = sessions.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(HarnessEvent::Session { agent, event, .. }) => {
                        let session = map.lock().unwrap().session_of(agent);
                        // Every session is durable. Root sessions are filed by id;
                        // subagents (whose ids restart each run) are filed under
                        // their root as `<root>.sub.<child>`.
                        if persist && let Some(h) = pump_harness.get(agent) {
                            let key = match h.live.parent {
                                None => session.clone(),
                                Some(_) => {
                                    let mut root = agent;
                                    while let Some(p) =
                                        pump_harness.get(root).and_then(|x| x.live.parent)
                                    {
                                        root = p;
                                    }
                                    format!("{}.sub.{session}", map.lock().unwrap().session_of(root))
                                }
                            };
                            let mut w = writers.lock().unwrap();
                            if !w.contains_key(&key) {
                                let opened = match h.live.parent {
                                    None => SessionWriter::open_with(
                                        &pump_cwd,
                                        &session,
                                        &h.live.model(),
                                        match &advisor {
                                            Some((a, s)) => json!({"advisor": {"advisor": a, "slave": s}}),
                                            None => json!({}),
                                        },
                                    ),
                                    Some(parent) => {
                                        let m = map.lock().unwrap();
                                        let mut root = parent;
                                        while let Some(p) =
                                            pump_harness.get(root).and_then(|x| x.live.parent)
                                        {
                                            root = p;
                                        }
                                        SessionWriter::open_child(
                                            &pump_cwd,
                                            &m.session_of(root),
                                            &session,
                                            &m.session_of(parent),
                                            &h.live.model(),
                                        )
                                    }
                                };
                                if let Ok(nw) = opened {
                                    w.insert(key.clone(), nw);
                                }
                            }
                            if let Some(wr) = w.get_mut(&key) {
                                wr.append(&event);
                            }
                        }
                        emit(
                            &out,
                            index,
                            Frame::notification(
                                notify::SESSION_EVENT,
                                json!({ "sessionId": session, "event": *event }),
                            ),
                        );
                    }
                    Ok(HarnessEvent::Status { agent, status }) => {
                        let session = map.lock().unwrap().session_of(agent);
                        emit(
                            &out,
                            index,
                            Frame::notification(
                                notify::SESSION_STATUS,
                                json!({ "sessionId": session, "status": status.as_str() }),
                            ),
                        );
                    }
                    Ok(HarnessEvent::Spawned { agent, parent: Some(parent), title }) => {
                        let m = map.lock().unwrap();
                        let parent_session = m.session_of(parent);
                        let child_session = m.session_of(agent);
                        drop(m);
                        emit(
                            &out,
                            index,
                            Frame::notification(
                                notify::SUBAGENT_STARTED,
                                json!({
                                    "parentSessionId": parent_session,
                                    "childSessionId": child_session,
                                    "title": title,
                                }),
                            ),
                        );
                    }
                    Ok(HarnessEvent::Finished { agent, status, last_message }) => {
                        let session = map.lock().unwrap().session_of(agent);
                        emit(
                            &out,
                            index,
                            Frame::notification(
                                notify::SUBAGENT_FINISHED,
                                json!({
                                    "childSessionId": session,
                                    "status": if matches!(status, destro_core::AgentStatus::Failed | destro_core::AgentStatus::Killed) { "failed" } else { "ok" },
                                    "lastAssistantMessage": last_message,
                                }),
                            ),
                        );
                    }
                    Ok(HarnessEvent::Spawned { parent: None, .. }) => {}
                    Ok(HarnessEvent::Approval { id, agent, tool, summary }) => {
                        let session = map.lock().unwrap().session_of(agent);
                        emit(
                            &out,
                            index,
                            Frame::Request {
                                id: json!(format!("a{id}")),
                                method: "approval/request".into(),
                                params: json!({
                                    "sessionId": session,
                                    "toolName": tool,
                                    "reason": summary,
                                }),
                            },
                        );
                    }
                    Ok(HarnessEvent::Question { id, agent, question, options }) => {
                        let session = map.lock().unwrap().session_of(agent);
                        emit(
                            &out,
                            index,
                            Frame::Request {
                                id: json!(format!("q{id}")),
                                method: "user-question/ask".into(),
                                params: json!({
                                    "sessionId": session,
                                    "question": question,
                                    "options": options,
                                }),
                            },
                        );
                    }
                    Ok(HarnessEvent::Delta { .. }) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Slot {
            harness,
            sessions,
            cwd: cwd.to_string(),
            name: name.to_string(),
            setup,
            mcp,
            model_override: Arc::new(Mutex::new(None)),
        }
    }

    fn with_slot<R>(&self, project: usize, f: impl FnOnce(&mut Slot) -> R) -> Option<R> {
        let mut slots = self.slots.lock().unwrap();
        while slots.len() <= project {
            slots.push(None);
        }
        slots[project].as_mut().map(f)
    }

    fn ensure_agent(&self, project: usize, session: &str) -> Option<AgentId> {
        self.with_slot(project, |slot| {
            if let Some(id) = slot.sessions.lock().unwrap().agent(session) {
                return Some(id);
            }
            let cwd = std::path::PathBuf::from(&slot.cwd);
            let model = slot.model_override.lock().unwrap().clone();
            let handle = self.rt.block_on(async {
                slot.harness.spawn(
                    None,
                    SpawnSpec {
                        title: Some(session.to_string()),
                        cwd: Some(cwd),
                        model,
                        ..SpawnSpec::default()
                    },
                )
            });
            match handle {
                Ok(handle) => {
                    slot.sessions.lock().unwrap().bind(session, handle.id);
                    Some(handle.id)
                }
                Err(e) => {
                    emit(
                        &self.tx,
                        project,
                        Frame::notification(notify::SESSION_EVENT, json!({
                            "sessionId": session,
                            "event": { "type": "notice", "data": { "text": format!("spawn failed: {e}") } }
                        })),
                    );
                    None
                }
            }
        })?
    }

    fn respond(&self, project: usize, id: Value, result: Value) {
        emit(&self.tx, project, Frame::Response { id, result });
    }
}

fn emit(tx: &Sender<Msg>, project: usize, frame: Frame) {
    let _ = tx.send(Msg::Frame {
        project,
        line: frame.to_line(),
    });
}

impl Backend for Local {
    fn send(&self, project: usize, frame: Value) {
        let method = frame["method"].as_str().unwrap_or("");
        // answers to approval/question requests we issued ("a<id>" / "q<id>")
        if method.is_empty()
            && let Some(rid) = frame["id"].as_str()
        {
            let num = rid[1..].parse::<u64>().ok();
            self.with_slot(project, |slot| {
                let i = &slot.harness.interact;
                match (rid.as_bytes().first(), num) {
                    (Some(b'a'), Some(n)) => i.resolve_approval(
                        n,
                        match frame["result"]["outcome"].as_str() {
                            Some("allowed-once") => Decision::Allow,
                            Some("allowed-session") => Decision::AllowSession,
                            _ => Decision::Reject,
                        },
                    ),
                    (Some(b'q'), Some(n)) => {
                        i.answer(n, frame["result"]["answer"].as_str().map(str::to_string))
                    }
                    _ => {}
                }
            });
            return;
        }
        let params = &frame["params"];
        let id = frame["id"].clone();
        match method {
            method::INITIALIZE => {
                let info = ServerInfo {
                    name: "destro-harness".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                };
                self.respond(
                    project,
                    id,
                    serde_json::to_value(destro_proto::InitializeResult { server_info: info })
                        .unwrap(),
                );
            }
            method::SESSION_QUERY | method::SESSION_PROMPT => {
                let session = params["sessionId"].as_str().unwrap_or("").to_string();
                let text = params["contentBlocks"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let Some(agent) = self.ensure_agent(project, &session) else {
                    return;
                };
                self.with_slot(project, |slot| {
                    if let Some(handle) = slot.harness.get(agent) {
                        let _ = handle.send(text, Delivery::Queue);
                    }
                });
                self.respond(
                    project,
                    id,
                    json!({ "kind": "prompt", "messageId": format!("u-{}", agent.0) }),
                );
            }
            method::SHUTDOWN => {
                self.with_slot(project, |slot| {
                    let h = slot.harness.clone();
                    self.rt.spawn(async move {
                        h.shutdown().await;
                    });
                });
                self.respond(project, id, json!({}));
            }
            ext::AGENT_LIST => {
                if let Some(agents) = self.with_slot(project, |slot| slot.harness.list()) {
                    self.respond(project, id, json!({ "agents": agents }));
                }
            }
            ext::AGENT_STATE => {
                let agent = params["agentId"].as_str().and_then(AgentId::parse);
                let state = agent.and_then(|a| {
                    self.with_slot(project, |slot| slot.harness.state(a))
                        .flatten()
                });
                self.respond(
                    project,
                    id,
                    state
                        .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                        .unwrap_or(Value::Null),
                );
            }
            ext::AGENT_SEND => {
                let agent = params["agentId"].as_str().and_then(AgentId::parse);
                let text = params["text"].as_str().unwrap_or("");
                let mode = Delivery::from_wire(params["mode"].as_str());
                let ok = agent
                    .and_then(|a| self.with_slot(project, |slot| slot.harness.get(a)))
                    .flatten()
                    .map(|h| h.send(text, mode).is_ok());
                self.respond(project, id, json!({ "ok": ok.unwrap_or(false) }));
            }
            ext::AGENT_INTERRUPT => {
                let agent = params["agentId"].as_str().and_then(AgentId::parse);
                if let Some(h) = agent
                    .and_then(|a| self.with_slot(project, |slot| slot.harness.get(a)))
                    .flatten()
                {
                    h.interrupt();
                }
                self.respond(project, id, json!({ "ok": true }));
            }
            ext::AGENT_KILL => {
                let killed = params["agentId"]
                    .as_str()
                    .and_then(AgentId::parse)
                    .and_then(|a| self.with_slot(project, |slot| slot.harness.kill(a)))
                    .and_then(|r| r.ok())
                    .unwrap_or(0);
                self.respond(project, id, json!({ "killed": killed }));
            }
            ext::AGENT_MODELS => {
                let model = self.model.clone();
                self.respond(
                    project,
                    id,
                    json!({ "models": [{ "id": model, "provider": "mock" }] }),
                );
            }
            _ => {
                // The TUI ignores these; answer anyway so nothing hangs.
                self.respond(project, id, json!({}));
            }
        }
    }

    fn kill(&self, project: usize) {
        self.with_slot(project, |slot| {
            let h = slot.harness.clone();
            self.rt.spawn(async move {
                h.shutdown().await;
            });
            let mut map = slot.sessions.lock().unwrap();
            map.by_session.clear();
            map.by_agent.clear();
        });
    }

    fn open(&self, project: usize, spec: &ProjectSpec) {
        let mut slots = self.slots.lock().unwrap();
        while slots.len() <= project {
            slots.push(None);
        }
        // Build the slot without holding the outer lock across spawn: create the
        // runtime task first, then install it.
        drop(slots);
        let slot = Self::slot(
            &self.rt,
            project,
            &spec.name,
            &spec.cwd,
            normal_setup(&self.model),
            self.providers.clone(),
            &self.tx,
            self.persist,
            self.approvals.load(std::sync::atomic::Ordering::Relaxed),
        );
        self.slots.lock().unwrap()[project] = Some(slot);
    }

    fn configure_advisor(&self, project: usize, advisor: &str, slave: &str) {
        let Some((name, cwd, old)) = self.with_slot(project, |slot| {
            (slot.name.clone(), slot.cwd.clone(), slot.harness.clone())
        }) else {
            return;
        };
        self.rt.spawn(async move { old.shutdown().await });
        let slot = Self::slot(
            &self.rt,
            project,
            &name,
            &cwd,
            advisor_setup(advisor, slave),
            self.providers.clone(),
            &self.tx,
            self.persist,
            self.approvals.load(std::sync::atomic::Ordering::Relaxed),
        );
        if let Some(s) = self.slots.lock().unwrap().get_mut(project) {
            *s = Some(slot);
        }
    }

    fn restore(&self, project: usize, session: &str, history: Vec<Value>, model: &str) {
        self.with_slot(project, |slot| {
            if slot.sessions.lock().unwrap().agent(session).is_some() {
                return;
            }
            let model = if model.is_empty() {
                slot.model_override.lock().unwrap().clone()
            } else {
                Some(model.to_string())
            };
            let spawned = self.rt.block_on(async {
                slot.harness.spawn(
                    None,
                    SpawnSpec {
                        title: Some(session.to_string()),
                        cwd: Some(std::path::PathBuf::from(&slot.cwd)),
                        model,
                        history,
                        ..SpawnSpec::default()
                    },
                )
            });
            if let Ok(h) = spawned {
                slot.sessions.lock().unwrap().bind(session, h.id);
            }
        });
    }

    fn set_model(&self, project: usize, session: Option<&str>, model: &str) {
        self.with_slot(project, |slot| {
            *slot.model_override.lock().unwrap() = Some(model.to_string());
            if let Some(agent) = session.and_then(|s| slot.sessions.lock().unwrap().agent(s))
                && let Some(h) = slot.harness.get(agent)
            {
                h.set_model(model);
            }
        });
    }

    fn mcp_status(&self, project: usize) -> Vec<McpStatus> {
        self.with_slot(project, |slot| slot.mcp.statuses())
            .unwrap_or_default()
    }

    fn mcp_do(&self, project: usize, server: &str, action: &str) {
        self.with_slot(project, |slot| match action {
            "approve" | "enable" => slot.mcp.set_enabled(server, true),
            "disable" => slot.mcp.set_enabled(server, false),
            "reconnect" => slot.mcp.reconnect(server),
            _ => {}
        });
    }

    fn interrupt(&self, project: usize, session: &str) {
        self.with_slot(project, |slot| {
            if let Some(agent) = slot.sessions.lock().unwrap().agent(session)
                && let Some(h) = slot.harness.get(agent)
            {
                h.interrupt();
            }
        });
    }

    fn set_approvals(&self, on: bool) {
        self.approvals
            .store(on, std::sync::atomic::Ordering::Relaxed);
        for slot in self.slots.lock().unwrap().iter().flatten() {
            slot.harness.interact.set_approvals(on);
        }
    }

    fn compact(&self, project: usize, session: &str) {
        self.with_slot(project, |slot| {
            if let Some(agent) = slot.sessions.lock().unwrap().agent(session)
                && let Some(h) = slot.harness.get(agent)
            {
                h.compact();
            }
        });
    }

    fn add_provider(&self, id: &str) -> Result<(), String> {
        let registry = destro_core::ProviderRegistry::load();
        let provider = registry.build(id)?;
        self.providers.insert(id, provider);
        Ok(())
    }

    fn mcp_prompts(&self, project: usize) -> Vec<destro_core::mcp::McpPrompt> {
        self.with_slot(project, |slot| slot.mcp.prompts())
            .unwrap_or_default()
    }

    fn mcp_prompt(
        &self,
        project: usize,
        server: &str,
        name: &str,
        args: serde_json::Map<String, Value>,
    ) -> Result<String, String> {
        let mcp = self
            .with_slot(project, |slot| slot.mcp.clone())
            .ok_or("no such workspace")?;
        self.rt.block_on(mcp.get_prompt(server, name, args))
    }
}
