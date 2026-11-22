//! Admission control for model calls.
//!
//! A thousand agents can be alive while only a handful of provider requests may
//! be in flight (rate limits, connection pools, cost). Agents queue here instead
//! of spawning unbounded work, and operator-driven turns keep reserved slots so a
//! wall of background subagents can never starve the prompt you just typed.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// A human-visible turn. May use every slot.
    Interactive,
    /// Subagent or other background work. Leaves `reserve` slots for interactive.
    Background,
}

struct Inner {
    running: usize,
}

/// A fair-as-possible counting gate with a reserved interactive lane.
pub struct Gate {
    inner: Mutex<Inner>,
    notify: Notify,
    max: usize,
    reserve: usize,
}

impl Gate {
    /// `max` total concurrent calls; `reserve` of them only for interactive work.
    pub fn new(max: usize, reserve: usize) -> Arc<Gate> {
        let max = max.max(1);
        Arc::new(Gate {
            inner: Mutex::new(Inner { running: 0 }),
            notify: Notify::new(),
            max,
            reserve: reserve.min(max.saturating_sub(1)),
        })
    }

    /// Wait for a slot. The returned permit releases on drop.
    pub async fn acquire(&self, priority: Priority) -> Permit<'_> {
        loop {
            // Register interest before checking, so a release between the check
            // and the await cannot be missed.
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.lock();
                let cap = match priority {
                    Priority::Interactive => self.max,
                    Priority::Background => self.max - self.reserve,
                };
                if inner.running < cap {
                    inner.running += 1;
                    return Permit { gate: self };
                }
            }
            notified.await;
        }
    }

    /// Slots currently held.
    pub fn running(&self) -> usize {
        self.inner.lock().running
    }

    pub fn max(&self) -> usize {
        self.max
    }

    fn release(&self) {
        {
            let mut inner = self.inner.lock();
            inner.running = inner.running.saturating_sub(1);
        }
        self.notify.notify_waiters();
    }
}

/// Held for the duration of one model request.
pub struct Permit<'a> {
    gate: &'a Gate,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn background_never_exceeds_max_minus_reserve() {
        let gate = Gate::new(3, 1);
        let mut permits = vec![];
        for _ in 0..2 {
            permits.push(gate.acquire(Priority::Background).await);
        }
        assert_eq!(gate.running(), 2);
        // The third background slot is reserved for interactive work.
        let extra = tokio::time::timeout(
            Duration::from_millis(20),
            gate.acquire(Priority::Background),
        )
        .await;
        assert!(
            extra.is_err(),
            "background should be blocked at cap-reserve"
        );

        let interactive = gate.acquire(Priority::Interactive).await;
        assert_eq!(gate.running(), 3);
        drop(interactive);
        drop(permits);
        assert_eq!(gate.running(), 0);
    }

    #[tokio::test]
    async fn permits_are_not_lost_under_contention() {
        let gate = Gate::new(2, 0);
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = vec![];
        for _ in 0..16 {
            let gate = gate.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(async move {
                let _p = gate.acquire(Priority::Interactive).await;
                peak.fetch_max(gate.running(), Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(gate.running(), 0);
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }
}
