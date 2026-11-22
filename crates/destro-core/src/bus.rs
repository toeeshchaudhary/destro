//! Event fan-out.
//!
//! Every state change an observer might care about is published once here. The
//! channel is bounded and lossy-by-lag on purpose: a UI subscribed to a thousand
//! agents must never apply backpressure to the agent loop. Subscribers that fall
//! behind get `Lagged` and re-sync from [`crate::Harness::list`].

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::broadcast;

use destro_proto::event::SessionEvent;

use crate::agent::{AgentId, AgentStatus};

#[derive(Clone, Debug)]
pub enum HarnessEvent {
    /// A new agent exists (root or subagent).
    Spawned {
        agent: AgentId,
        parent: Option<AgentId>,
        title: Option<String>,
    },
    /// Whole-agent status transition.
    Status { agent: AgentId, status: AgentStatus },
    /// One committed session event, already in its wire envelope.
    Session {
        agent: AgentId,
        session_id: Arc<str>,
        event: Arc<Value>,
    },
    /// Streaming text, for live transcripts. Not part of the durable log.
    Delta {
        agent: AgentId,
        reasoning: bool,
        text: Arc<str>,
    },
    /// The agent will produce no more events.
    Finished {
        agent: AgentId,
        status: AgentStatus,
        last_message: Option<String>,
    },
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<HarnessEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> EventBus {
        let (tx, _) = broadcast::channel(capacity.max(16));
        EventBus { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HarnessEvent> {
        self.tx.subscribe()
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn publish(&self, event: HarnessEvent) {
        let _ = self.tx.send(event);
    }

    /// Publish a durable session event, serialising only when someone listens.
    pub fn session(&self, agent: AgentId, session_id: &Arc<str>, event: SessionEvent) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        self.publish(HarnessEvent::Session {
            agent,
            session_id: session_id.clone(),
            event: Arc::new(event.to_value()),
        });
    }

    pub fn delta(&self, agent: AgentId, reasoning: bool, text: &str) {
        if self.tx.receiver_count() == 0 || text.is_empty() {
            return;
        }
        self.publish(HarnessEvent::Delta {
            agent,
            reasoning,
            text: Arc::from(text),
        });
    }
}
