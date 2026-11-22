//! Operator interaction: approvals and questions an agent waits on.
//!
//! The agent loop asks, a client (the TUI) answers. Neither side blocks the
//! other: a request registers a one-shot channel here, publishes a bus event,
//! and the client resolves it by id whenever the user decides.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::oneshot;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Allow,
    /// Allow this tool for the rest of the run without asking again.
    AllowSession,
    Reject,
}

#[derive(Default)]
pub struct Interact {
    next: AtomicU64,
    attached: AtomicBool,
    approvals_on: AtomicBool,
    approvals: Mutex<HashMap<u64, (String, oneshot::Sender<Decision>)>>,
    questions: Mutex<HashMap<u64, oneshot::Sender<Option<String>>>>,
    allowed: Mutex<HashSet<String>>,
}

impl Interact {
    /// A client that can answer is connected. Without one, `ask_user` fails fast
    /// instead of hanging forever.
    pub fn attach(&self) {
        self.attached.store(true, Ordering::Relaxed);
    }

    pub fn attached(&self) -> bool {
        self.attached.load(Ordering::Relaxed)
    }

    pub fn set_approvals(&self, on: bool) {
        self.approvals_on.store(on, Ordering::Relaxed);
        if !on {
            self.allowed.lock().clear();
        }
    }

    pub fn approvals_on(&self) -> bool {
        self.approvals_on.load(Ordering::Relaxed) && self.attached()
    }

    /// Tools that change things outside the conversation: shell, file writes and
    /// MCP tools (their effects are unknown). Reading resources is exempt.
    pub fn needs_approval(tool: &str) -> bool {
        match tool {
            "pwsh" | "write" | "edit" => true,
            t if t.starts_with("mcp__") => {
                !(t.ends_with("__list_resources") || t.ends_with("__read_resource"))
            }
            _ => false,
        }
    }

    pub fn is_allowed(&self, tool: &str) -> bool {
        self.allowed.lock().contains(tool)
    }

    /// Register an approval request; the receiver resolves with the decision.
    pub fn request_approval(&self, tool: &str) -> (u64, oneshot::Receiver<Decision>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.approvals.lock().insert(id, (tool.to_string(), tx));
        (id, rx)
    }

    pub fn resolve_approval(&self, id: u64, decision: Decision) {
        if let Some((tool, tx)) = self.approvals.lock().remove(&id) {
            if decision == Decision::AllowSession {
                self.allowed.lock().insert(tool);
            }
            let _ = tx.send(decision);
        }
    }

    /// Forget an approval whose agent gave up waiting (interrupted).
    pub fn forget_approval(&self, id: u64) {
        self.approvals.lock().remove(&id);
    }

    pub fn request_question(&self) -> (u64, oneshot::Receiver<Option<String>>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.questions.lock().insert(id, tx);
        (id, rx)
    }

    /// `None` means the user skipped the question.
    pub fn answer(&self, id: u64, answer: Option<String>) {
        if let Some(tx) = self.questions.lock().remove(&id) {
            let _ = tx.send(answer);
        }
    }

    pub fn forget_question(&self, id: u64) {
        self.questions.lock().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_need_an_attached_client() {
        let i = Interact::default();
        i.set_approvals(true);
        assert!(!i.approvals_on(), "no client attached yet");
        i.attach();
        assert!(i.approvals_on());
    }

    #[test]
    fn session_allow_sticks_until_approvals_are_turned_off() {
        let i = Interact::default();
        let (id, mut rx) = i.request_approval("pwsh");
        i.resolve_approval(id, Decision::AllowSession);
        assert_eq!(rx.try_recv().unwrap(), Decision::AllowSession);
        assert!(i.is_allowed("pwsh"));
        i.set_approvals(false);
        assert!(!i.is_allowed("pwsh"));
    }

    #[test]
    fn which_tools_need_approval() {
        assert!(Interact::needs_approval("pwsh"));
        assert!(Interact::needs_approval("mcp__git__commit"));
        assert!(!Interact::needs_approval("mcp__git__read_resource"));
        assert!(!Interact::needs_approval("read"));
        assert!(!Interact::needs_approval("web_fetch"));
    }

    #[test]
    fn answers_route_by_id() {
        let i = Interact::default();
        let (id, mut rx) = i.request_question();
        i.answer(id, Some("yes".into()));
        assert_eq!(rx.try_recv().unwrap(), Some("yes".to_string()));
        i.answer(id, None); // second answer is ignored
    }
}
