//! Scale gate: many concurrent agents on one runtime.
//!
//! These are assertions of shape and liveness, plus timing printed for the record.
//! They intentionally avoid absolute latency thresholds, which belong to a
//! dedicated benchmark run rather than to CI on an arbitrary machine.

use std::sync::Arc;
use std::time::Instant;

use destro_core::{Harness, HarnessConfig, MockProvider, SpawnSpec};

fn harness(provider: Arc<MockProvider>, concurrency: usize) -> Arc<Harness> {
    let mut cfg = HarnessConfig::new(provider);
    cfg.concurrency = concurrency;
    cfg.interactive_reserve = 4;
    cfg.cwd = std::env::temp_dir();
    Harness::new(cfg)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thousand_agents_run_to_idle() {
    let provider = Arc::new(MockProvider::new(|req| {
        destro_core::MockTurn::text(format!("done: {}", destro_core::last_user_text(req)))
    }));
    let h = harness(provider, 128);

    let started = Instant::now();
    let handles: Vec<_> = (0..1000)
        .map(|i| {
            h.spawn(
                None,
                SpawnSpec {
                    title: Some(format!("worker {i}")),
                    prompt: Some(format!("task {i}")),
                    ..SpawnSpec::default()
                },
            )
            .unwrap()
        })
        .collect();
    let spawned = started.elapsed();

    for handle in &handles {
        handle.wait().await;
    }
    let total = started.elapsed();
    let stats = h.stats();
    assert_eq!(stats.agents, 1000);
    assert_eq!(stats.idle, 1000, "stats: {stats:?}");
    assert!(handles.iter().all(|h| h.live.last_message().is_some()));

    eprintln!(
        "1000 agents: spawn {spawned:.1?}, all turns complete {total:.1?} ({:.0} agents/s)",
        1000.0 / total.as_secs_f64()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deep_tree_stays_addressable() {
    let h = harness(Arc::new(MockProvider::silent()), 16);
    let root = h.spawn(None, SpawnSpec::default()).unwrap();
    let mut parent = root.clone();
    for depth in 0..50 {
        parent = h
            .spawn(
                Some(parent.id),
                SpawnSpec {
                    title: Some(format!("depth {depth}")),
                    ..SpawnSpec::default()
                },
            )
            .unwrap();
    }
    assert_eq!(h.descendants(root.id).len(), 50);
    assert_eq!(h.list().len(), 51);
    assert_eq!(root.live.children(), 1);

    // Every node is reachable and steerable by id.
    let leaf = parent.clone();
    leaf.send("final word", destro_core::Delivery::Queue)
        .unwrap();
    leaf.wait().await;
    assert_eq!(h.state(leaf.id).unwrap().status, "idle");

    h.kill(root.id).unwrap();
    for _ in 0..200 {
        if h.stats().killed == 51 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(h.stats().killed, 51);
}
