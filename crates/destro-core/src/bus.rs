//! Event fan-out.
//!
//! Every state change an observer might care about is published once here. The
//! channel is bounded and lossy-by-lag on purpose: a UI subscribed to a thousand
//! agents must never apply backpressure to the agent loop. Subscribers that fall
//! behind get `Lagged` and re-sync from [`crate::Harness::list`].

use std::sync::{Arc, OnceLock};

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
    /// An agent wants to run a tool and is waiting for the operator's decision.
    Approval {
        id: u64,
        agent: AgentId,
        tool: String,
        summary: String,
    },
    /// An agent asked the operator a question and is waiting for the answer.
    Question {
        id: u64,
        agent: AgentId,
        question: String,
        options: Vec<String>,
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
    session_sink: Arc<OnceLock<Box<SessionSink>>>,
}

type SessionSink = dyn Fn(AgentId, &Value) + Send + Sync;

impl EventBus {
    pub fn new(capacity: usize) -> EventBus {
        let (tx, _) = broadcast::channel(capacity.max(16));
        EventBus {
            tx,
            session_sink: Arc::default(),
        }
    }

    /// Install a synchronous session sink before starting agents.
    pub fn set_session_sink(&self, sink: impl Fn(AgentId, &Value) + Send + Sync + 'static) {
        assert!(self.session_sink.set(Box::new(sink)).is_ok());
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

    /// Save a session event before delivering it to live observers.
    pub fn session(&self, agent: AgentId, session_id: &Arc<str>, event: SessionEvent) {
        let sink = self.session_sink.get();
        if sink.is_none() && self.tx.receiver_count() == 0 {
            return;
        }
        let event = event.to_value();
        if let Some(save) = sink {
            save(agent, &event);
        }
        self.publish(HarnessEvent::Session {
            agent,
            session_id: session_id.clone(),
            event: Arc::new(event),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, SessionWriter};
    use parking_lot::Mutex;

    #[test]
    fn lagging_observer_does_not_lose_saved_events() {
        let path = std::env::temp_dir().join(format!("destro-bus-{}.jsonl", std::process::id()));
        let writer =
            Mutex::new(SessionWriter::open_at(&path, "/w", "chat", "m", "destro").unwrap());
        let bus = EventBus::new(16);
        bus.set_session_sink(move |_, event| writer.lock().append(event).unwrap());
        let mut observer = bus.subscribe();
        let session: Arc<str> = "chat".into();
        for i in 0..64 {
            bus.session(
                AgentId(1),
                &session,
                SessionEvent::Notice {
                    text: i.to_string(),
                },
            );
        }
        assert!(matches!(
            observer.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let events = store::load(&path);
        drop(bus);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(events.len(), 64);
        assert_eq!(events[63]["data"]["text"], "63");
    }
}
