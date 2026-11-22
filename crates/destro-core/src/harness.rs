//! The harness: a registry of live agents plus the control plane over them.
//!
//! One harness owns one runtime's worth of agents. Spawning is O(1); listing is a
//! sequence of atomic reads; steering goes through each agent's mailbox, so the
//! registry never sits between the operator and a working agent's loop.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use destro_proto::{AgentState, Delivery};
use destro_tools::Registry;

use crate::agent::{
    AgentHandle, AgentId, AgentInit, AgentStatus, AgentTask, Command, Live, MAILBOX,
};
use crate::bus::{EventBus, HarnessEvent};
use crate::gate::{Gate, Priority};
use crate::provider::Provider;
use crate::tools_ctl::{ToolHost, control_tools};

/// How to create one agent.
#[derive(Clone, Debug, Default)]
pub struct SpawnSpec {
    pub title: Option<String>,
    pub model: Option<String>,
    /// Extra system prompt appended to the harness's base prompt.
    pub system: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Send this as the agent's first user message.
    pub prompt: Option<String>,
    pub priority: Option<Priority>,
    /// Prior conversation (neutral chat messages) to start from: resume.
    pub history: Vec<serde_json::Value>,
}

/// Providers addressable by a `provider/model` route string.
///
/// Advisor mode needs an advisor on one vendor and workers on another; a single
/// provider handle cannot express that. `resolve` splits a prefixed model, else
/// falls back to the default provider and an unprefixed model.
pub struct ProviderSet {
    providers: HashMap<String, Arc<dyn Provider>>,
    default: String,
}

impl ProviderSet {
    pub fn new(
        default: impl Into<String>,
        providers: HashMap<String, Arc<dyn Provider>>,
    ) -> Arc<ProviderSet> {
        Arc::new(ProviderSet {
            providers,
            default: default.into(),
        })
    }

    /// `(provider, model)`. Accepts `vendor/model` or `vendor:model`; unknown
    /// prefixes stay part of the model name.
    pub fn resolve(&self, model: &str) -> (Arc<dyn Provider>, String) {
        if let Some((prefix, rest)) = model.split_once('/').or_else(|| model.split_once(':'))
            && let Some(provider) = self.providers.get(prefix)
        {
            return (provider.clone(), rest.to_string());
        }
        (self.default_provider(), model.to_string())
    }

    pub fn default_provider(&self) -> Arc<dyn Provider> {
        self.providers
            .get(&self.default)
            .or_else(|| self.providers.values().next())
            .cloned()
            .unwrap_or_else(|| Arc::new(crate::provider::MockProvider::echo()))
    }

    pub fn ids(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }
}

#[derive(Clone)]
pub struct HarnessConfig {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub system: String,
    pub cwd: PathBuf,
    pub max_tokens: Option<u64>,
    /// Model route for child agents. Advisor mode sets the slaves' model here.
    pub slave_model: Option<String>,
    /// System prompt for child agents; both advisor and workers get their own.
    pub slave_system: Option<String>,
    /// Multi-provider routing. When set, `provider/model` route strings resolve.
    pub providers: Option<Arc<ProviderSet>>,
    /// Model requests allowed per turn (tool steps).
    pub max_steps: u32,
    /// Concurrent provider requests across all agents.
    pub concurrency: usize,
    /// Slots inside `concurrency` reserved for interactive turns.
    pub interactive_reserve: usize,
    pub event_buffer: usize,
}

impl HarnessConfig {
    pub fn new(provider: Arc<dyn Provider>) -> HarnessConfig {
        HarnessConfig {
            provider,
            model: "deepseek-v4-pro".into(),
            system: String::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            max_tokens: None,
            max_steps: 64,
            slave_model: None,
            slave_system: None,
            providers: None,
            concurrency: 24,
            interactive_reserve: 4,
            event_buffer: 8192,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HarnessStats {
    pub agents: usize,
    pub running: usize,
    pub idle: usize,
    pub failed: usize,
    pub killed: usize,
}

pub struct Harness {
    agents: RwLock<BTreeMap<AgentId, AgentHandle>>,
    next: AtomicU64,
    pub bus: EventBus,
    pub gate: Arc<Gate>,
    pub tools: Arc<Registry>,
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub system: String,
    pub cwd: PathBuf,
    pub max_tokens: Option<u64>,
    pub max_steps: u32,
    pub slave_model: Option<String>,
    pub slave_system: Option<String>,
    pub providers: Option<Arc<ProviderSet>>,
}

impl Harness {
    pub fn new(cfg: HarnessConfig) -> Arc<Harness> {
        let tool_host: ToolHost = Arc::new(OnceLock::new());
        let registry = Registry::builtin();
        for tool in control_tools(tool_host.clone()) {
            registry.register(tool);
        }
        let gate = Gate::new(cfg.concurrency, cfg.interactive_reserve);
        let harness = Arc::new(Harness {
            agents: RwLock::new(BTreeMap::new()),
            next: AtomicU64::new(0),
            bus: EventBus::new(cfg.event_buffer),
            gate,
            tools: Arc::new(registry),
            provider: cfg.provider,
            model: cfg.model,
            system: cfg.system,
            cwd: cfg.cwd,
            max_tokens: cfg.max_tokens,
            max_steps: cfg.max_steps.max(1),
            slave_model: cfg.slave_model,
            slave_system: cfg.slave_system,
            providers: cfg.providers,
        });
        let _ = tool_host.set(Arc::downgrade(&harness));
        harness
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HarnessEvent> {
        self.bus.subscribe()
    }

    /// Create an agent. Requires a Tokio runtime: the agent is a spawned task.
    pub fn spawn(
        self: &Arc<Self>,
        parent: Option<AgentId>,
        spec: SpawnSpec,
    ) -> Result<AgentHandle, String> {
        let parent_handle = match parent {
            Some(id) => Some(
                self.get(id)
                    .ok_or_else(|| format!("unknown parent agent {id}"))?,
            ),
            None => None,
        };
        let id = AgentId(self.next.fetch_add(1, Ordering::SeqCst));
        // Children inherit the slave route when one is configured; a root agent
        // stays on the primary route. This is what makes advisor mode work
        // without changing the delegation tool.
        let default_model = if parent.is_some() {
            self.slave_model
                .clone()
                .unwrap_or_else(|| self.model.clone())
        } else {
            self.model.clone()
        };
        let requested = spec.model.unwrap_or(default_model);
        let (provider, model) = match &self.providers {
            Some(set) => set.resolve(&requested),
            None => (self.provider.clone(), requested),
        };
        let live = Live::new(id, parent, &model, spec.title.clone());
        let parent_live = parent_handle.as_ref().map(|h| h.live.clone());
        let cancel = match &parent_handle {
            Some(h) => h.cancel.child_token(),
            None => CancellationToken::new(),
        };
        let priority = spec.priority.unwrap_or(if parent.is_some() {
            Priority::Background
        } else {
            Priority::Interactive
        });
        let (tx, rx) = tokio::sync::mpsc::channel::<Command>(MAILBOX);
        live.attach_cancel(cancel.clone());
        let handle = AgentHandle::new(id, live.clone(), cancel.clone(), priority, tx);

        let base_system = if parent.is_some() {
            self.slave_system
                .clone()
                .unwrap_or_else(|| self.system.clone())
        } else {
            self.system.clone()
        };
        let system = match spec.system {
            Some(extra) if base_system.is_empty() => extra,
            Some(extra) => format!("{base_system}\n\n{extra}"),
            None => base_system,
        };
        let init = AgentInit {
            id,
            live,
            parent_live,
            provider: provider.clone(),
            tools: self.tools.clone(),
            gate: self.gate.clone(),
            bus: self.bus.clone(),
            cwd: spec.cwd.unwrap_or_else(|| self.cwd.clone()),
            system,
            model,
            max_tokens: self.max_tokens,
            max_steps: self.max_steps,
            priority,
            history: spec.history,
            providers: self.providers.clone(),
        };
        tokio::spawn(AgentTask::new(init, cancel).run(rx));

        self.agents.write().insert(id, handle.clone());
        if let Some(p) = &parent_handle {
            p.live.bump_children(1);
        }
        self.bus.publish(HarnessEvent::Spawned {
            agent: id,
            parent,
            title: spec.title,
        });
        if let Some(prompt) = spec.prompt {
            handle.send(prompt, Delivery::Queue)?;
        }
        Ok(handle)
    }

    pub fn get(&self, id: AgentId) -> Option<AgentHandle> {
        self.agents.read().get(&id).cloned()
    }

    /// Inspector snapshot of one agent.
    pub fn state(&self, id: AgentId) -> Option<AgentState> {
        self.agents.read().get(&id).map(|h| h.live.snapshot(id))
    }

    /// Inspector snapshot of every agent, in creation order.
    pub fn list(&self) -> Vec<AgentState> {
        let agents = self.agents.read();
        agents.values().map(|h| h.live.snapshot(h.id)).collect()
    }

    pub fn roots(&self) -> Vec<AgentHandle> {
        self.agents
            .read()
            .values()
            .filter(|h| h.live.parent.is_none())
            .cloned()
            .collect()
    }

    pub fn children(&self, id: AgentId) -> Vec<AgentHandle> {
        self.agents
            .read()
            .values()
            .filter(|h| h.live.parent == Some(id))
            .cloned()
            .collect()
    }

    /// Every descendant of `id`, depth first, not including `id`.
    pub fn descendants(&self, id: AgentId) -> Vec<AgentId> {
        let agents = self.agents.read();
        let mut out = Vec::new();
        let mut stack: Vec<AgentId> = agents
            .values()
            .filter(|h| h.live.parent == Some(id))
            .map(|h| h.id)
            .collect();
        while let Some(next) = stack.pop() {
            out.push(next);
            stack.extend(
                agents
                    .values()
                    .filter(|h| h.live.parent == Some(next))
                    .map(|h| h.id),
            );
        }
        out
    }

    /// Stop an agent and its descendants. Returns how many agents were signalled.
    pub fn kill(&self, id: AgentId) -> Result<usize, String> {
        let handle = self.get(id).ok_or_else(|| format!("unknown agent {id}"))?;
        let count = 1 + self.descendants(id).len();
        handle.kill();
        Ok(count)
    }

    pub async fn wait(&self, id: AgentId) -> Result<AgentState, String> {
        let handle = self.get(id).ok_or_else(|| format!("unknown agent {id}"))?;
        Ok(handle.wait().await)
    }

    pub fn stats(&self) -> HarnessStats {
        let agents = self.agents.read();
        let mut stats = HarnessStats::default();
        for h in agents.values() {
            stats.agents += 1;
            match h.live.status() {
                AgentStatus::Idle => stats.idle += 1,
                AgentStatus::Running => stats.running += 1,
                AgentStatus::Failed => stats.failed += 1,
                AgentStatus::Killed => stats.killed += 1,
            }
        }
        stats
    }

    /// Kill every root agent and wait for all loops to settle.
    pub async fn shutdown(&self) {
        let roots: Vec<AgentId> = self.roots().iter().map(|h| h.id).collect();
        for id in roots {
            let _ = self.kill(id);
        }
        let handles: Vec<AgentHandle> = self.agents.read().values().cloned().collect();
        for h in handles {
            h.live.wait_idle().await;
        }
    }
}
