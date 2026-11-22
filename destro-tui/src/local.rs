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
}

impl Local {
    pub fn start(
        specs: &[ProjectSpec],
        model: &str,
        provider: Arc<ProviderSet>,
        tx: Sender<Msg>,
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
            )));
        }
        let backend = Local {
            rt,
            slots: Mutex::new(slots),
            tx,
            model: model.to_string(),
            providers: provider,
        };
        let projects = specs
            .iter()
            .map(|s| Project::new(&s.name, &s.cwd, model))
            .collect();
        (projects, backend)
    }

    fn slot(
        rt: &tokio::runtime::Runtime,
        index: usize,
        name: &str,
        cwd: &str,
        setup: Setup,
        providers: Arc<ProviderSet>,
        tx: &Sender<Msg>,
    ) -> Slot {
        let mut cfg = HarnessConfig::new(providers.default_provider());
        cfg.providers = Some(providers);
        cfg.model = setup.model.clone();
        cfg.cwd = std::path::PathBuf::from(cwd);
        cfg.system = setup.system.clone();
        cfg.slave_model = setup.slave_model.clone();
        cfg.slave_system = setup.slave_system.clone();
        let harness = Harness::new(cfg);
        let sessions: Sessions = Arc::new(Mutex::new(Map::default()));
        let mut rx = harness.subscribe();
        let out = tx.clone();
        let map = sessions.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(HarnessEvent::Session { agent, event, .. }) => {
                        let session = map.lock().unwrap().session_of(agent);
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
            let handle = self.rt.block_on(async {
                slot.harness.spawn(
                    None,
                    SpawnSpec {
                        title: Some(session.to_string()),
                        cwd: Some(cwd),
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
        );
        if let Some(s) = self.slots.lock().unwrap().get_mut(project) {
            *s = Some(slot);
        }
    }
}
