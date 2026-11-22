//! Destro's agent runtime: the loop, the tools, and the control plane.
//!
//! One [`Harness`] owns many agents. Every agent is an async task with a bounded
//! mailbox; [`AgentHandle`] is the cheap cloneable way to inspect or steer it.
//! The model-facing tool loop lives in [`agent`], concrete tools come from
//! `destro-tools`, and delegation/control tools live in [`tools_ctl`].

pub mod advisor;
pub mod agent;
pub mod bus;
pub mod context;
pub mod dsh;
pub mod gate;
pub mod harness;
pub mod interact;
pub mod mcp;
pub mod provider;
pub mod skills;
pub mod store;
pub mod tools_ctl;

pub use advisor::{advisor_system, slave_system};
pub use agent::{AgentHandle, AgentId, AgentStatus};
pub use bus::{EventBus, HarnessEvent};
pub use destro_proto::Delivery;
pub use gate::{Gate, Priority};
pub use harness::{Harness, HarnessConfig, HarnessStats, ProviderSet, SpawnSpec};
pub use interact::{Decision, Interact};
pub use provider::{
    ChatRequest, MockProvider, MockToolCall, MockTurn, OpenAiCompatProvider, Provider,
    ProviderDescriptor, ProviderError, ProviderInfo, ProviderItem, ProviderKind, ProviderRegistry,
    ProviderStream, SseEvent, SseParser, key_of, last_user_text, save_key, user_message,
};
