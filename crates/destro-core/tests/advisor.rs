//! Advisor mode: one planner/judge, many workers, distinct model routes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use destro_core::{
    Harness, HarnessConfig, MockProvider, MockToolCall, MockTurn, SpawnSpec, advisor,
};
use serde_json::json;

fn tmp_cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[tokio::test]
async fn advisor_delegates_to_slaves_on_the_slave_route() {
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let provider = Arc::new(MockProvider::new(move |req| {
        record
            .lock()
            .unwrap()
            .push((req.model.clone(), req.system.clone()));
        let has_tool_result = req.messages.iter().any(|m| m["role"] == "tool");
        if req.model == "advisor-x" && !has_tool_result {
            MockTurn::default().with_tool_call(MockToolCall::new(
                "call-1",
                "subagent",
                json!({"prompt": "implement the subtask and verify it", "title": "worker"}),
            ))
        } else if req.model == "advisor-x" {
            MockTurn::text("Outcome: done. Evidence: worker verified the change.")
        } else {
            MockTurn::text("Result: subtask done. Evidence: cargo test passed.")
        }
    }));

    let mut cfg = HarnessConfig::new(provider);
    cfg.model = "advisor-x".into();
    cfg.system = advisor::advisor_system("advisor-x", "slave-y");
    cfg.slave_model = Some("slave-y".into());
    cfg.slave_system = Some(advisor::slave_system());
    cfg.cwd = tmp_cwd();
    cfg.concurrency = 4;
    let harness = Harness::new(cfg);

    let root = harness
        .spawn(
            None,
            SpawnSpec {
                prompt: Some("build the feature".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), root.wait())
        .await
        .expect("advisor finished");
    assert_eq!(outcome.status, "idle");
    assert_eq!(
        outcome.last_message.as_deref(),
        Some("Outcome: done. Evidence: worker verified the change.")
    );

    let agents = harness.list();
    assert_eq!(agents.len(), 2, "one advisor plus one worker");
    let worker = agents
        .iter()
        .find(|a| a.parent_id.is_some())
        .expect("worker exists");
    assert_eq!(worker.model, "slave-y");
    assert_eq!(worker.status, "idle");

    let systems = seen.lock().unwrap();
    assert!(
        systems
            .iter()
            .any(|(model, system)| model == "slave-y"
                && system.contains("subagent executing one task")),
        "worker must get the slave prompt"
    );
    assert!(
        systems
            .iter()
            .any(|(model, system)| model == "advisor-x" && system.contains("You are the advisor")),
        "advisor must get the advisor prompt"
    );
    // Every request names a model the harness knows how to route.
    assert!(
        systems
            .iter()
            .all(|(model, _)| model == "advisor-x" || model == "slave-y")
    );
}

#[tokio::test]
async fn advisor_mode_fans_out_and_collects_parallel_workers() {
    let provider = Arc::new(MockProvider::new(|req| {
        let tool_results = req.messages.iter().filter(|m| m["role"] == "tool").count();
        if req.model == "boss" && tool_results == 0 {
            // Two fire-and-forget delegations, then the advisor synthesises.
            MockTurn::default()
                .with_tool_call(MockToolCall::new(
                    "a",
                    "subagent",
                    json!({"prompt": "task A", "wait": false}),
                ))
                .with_tool_call(MockToolCall::new(
                    "b",
                    "subagent",
                    json!({"prompt": "task B", "wait": false}),
                ))
        } else if req.model == "boss" {
            MockTurn::text("Outcome: A and B complete.")
        } else {
            MockTurn::text("worker done")
        }
    }));
    let mut cfg = HarnessConfig::new(provider);
    cfg.model = "boss".into();
    cfg.slave_model = Some("minion".into());
    cfg.cwd = tmp_cwd();
    cfg.max_steps = 4;
    let harness = Harness::new(cfg);

    let root = harness
        .spawn(
            None,
            SpawnSpec {
                prompt: Some("delegate A and B".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();
    root.wait().await;
    assert_eq!(
        root.state().await.last_message.as_deref(),
        Some("Outcome: A and B complete.")
    );

    // Workers were spawned without blocking the advisor; wait for them here.
    let worker_ids: Vec<_> = harness
        .list()
        .iter()
        .filter_map(|a| a.parent_id.as_deref().and_then(destro_core::AgentId::parse))
        .collect();
    assert_eq!(worker_ids.len(), 2, "both fire-and-forget workers exist");
    for id in worker_ids {
        harness.wait(id).await;
    }
    assert!(harness.list().iter().all(|a| a.status == "idle"));
}
