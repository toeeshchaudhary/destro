//! End-to-end behaviour of the agent loop and the steering control plane.

use std::sync::Arc;
use std::time::Duration;

use destro_core::{
    AgentStatus, ChatRequest, Delivery, Harness, HarnessConfig, HarnessEvent, MockProvider,
    MockToolCall, MockTurn, SpawnSpec, last_user_text,
};
use serde_json::json;

fn harness(provider: Arc<MockProvider>) -> Arc<Harness> {
    let mut cfg = HarnessConfig::new(provider);
    cfg.model = "mock-1".into();
    cfg.cwd = std::env::temp_dir();
    cfg.max_steps = 8;
    cfg.concurrency = 8;
    cfg.interactive_reserve = 1;
    Harness::new(cfg)
}

fn echo_reply(req: &ChatRequest) -> MockTurn {
    MockTurn::text(format!("reply to: {}", last_user_text(req)))
}

#[tokio::test]
async fn one_prompt_produces_a_turn_and_goes_idle() {
    let h = harness(Arc::new(MockProvider::new(echo_reply)));
    let mut events = h.subscribe();
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                prompt: Some("hello".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();

    let state = agent.wait().await;
    assert_eq!(state.status, "idle");
    assert_eq!(state.turn, 1);
    assert_eq!(state.last_message.as_deref(), Some("reply to: hello"));

    let mut kinds = vec![];
    while let Ok(ev) = events.try_recv() {
        if let HarnessEvent::Session { event, .. } = ev {
            kinds.push(event["type"].as_str().unwrap_or("").to_string());
        }
    }
    assert!(kinds.contains(&"turn/start".to_string()));
    assert!(kinds.contains(&"user/message".to_string()));
    assert!(kinds.contains(&"assistant/message".to_string()));
    assert!(kinds.contains(&"turn/end".to_string()));
}

#[tokio::test]
async fn tool_calls_round_trip_through_the_registry() {
    let provider = Arc::new(MockProvider::new(|req| {
        let has_tool_result = req.messages.iter().any(|m| m["role"] == "tool");
        if has_tool_result {
            MockTurn::text("done after echo")
        } else {
            MockTurn::default().with_tool_call(MockToolCall::new(
                "call-1",
                "echo",
                json!({"probe": 7}),
            ))
        }
    }));
    let h = harness(provider);
    let mut events = h.subscribe();
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                prompt: Some("use the echo tool".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();

    let state = agent.wait().await;
    assert_eq!(state.status, "idle");
    assert_eq!(state.last_message.as_deref(), Some("done after echo"));
    assert!(state.turn >= 1);

    let mut saw_call = false;
    let mut saw_result = false;
    while let Ok(ev) = events.try_recv() {
        if let HarnessEvent::Session { event, .. } = ev {
            match event["type"].as_str() {
                Some("tool/call") => saw_call = true,
                Some("tool/result") => {
                    saw_result = true;
                    assert_eq!(event["data"]["message"]["isError"], false);
                    assert!(
                        event["data"]["message"]["content"][0]["text"]
                            .as_str()
                            .unwrap()
                            .contains("probe")
                    );
                }
                _ => {}
            }
        }
    }
    assert!(saw_call && saw_result);
}

#[tokio::test]
async fn inject_preempts_a_running_turn() {
    // Slow chunks keep the agent visibly running while we steer it.
    let provider = Arc::new(
        MockProvider::new(echo_reply)
            .with_chunk_chars(4)
            .with_delay(Duration::from_millis(15)),
    );
    let h = harness(provider);
    let mut events = h.subscribe();
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                prompt: Some("original".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();

    // Wait until the first turn is definitely in flight.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(agent.status(), AgentStatus::Running);
    agent.send("steer now", Delivery::Inject).unwrap();

    let state = agent.wait().await;
    assert_eq!(state.status, "idle");
    assert_eq!(state.last_message.as_deref(), Some("reply to: steer now"));
    assert!(
        state.turn >= 2,
        "injection starts a fresh turn, got {}",
        state.turn
    );

    let mut interrupted = 0;
    while let Ok(ev) = events.try_recv() {
        if let HarnessEvent::Session { event, .. } = ev {
            if event["type"] == "turn/end" && event["data"]["reason"]["kind"] == "interrupted" {
                interrupted += 1;
            }
        }
    }
    assert!(interrupted >= 1, "expected an interrupted turn");
}

#[tokio::test]
async fn queued_message_waits_for_the_current_turn() {
    let provider = Arc::new(
        MockProvider::new(echo_reply)
            .with_chunk_chars(4)
            .with_delay(Duration::from_millis(5)),
    );
    let h = harness(provider);
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                prompt: Some("first".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;
    let running_turn = agent.live.turn();
    agent.send("second", Delivery::Queue).unwrap();

    let state = agent.wait().await;
    assert_eq!(state.last_message.as_deref(), Some("reply to: second"));
    assert!(
        state.turn > running_turn,
        "queued message ran only after the running turn ({running_turn})"
    );
}

#[tokio::test]
async fn interrupt_leaves_the_agent_alive_and_addressable() {
    let provider = Arc::new(
        MockProvider::new(echo_reply)
            .with_chunk_chars(2)
            .with_delay(Duration::from_millis(10)),
    );
    let h = harness(provider);
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                prompt: Some("long".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;

    agent.interrupt();
    let after = agent.wait().await;
    assert_eq!(after.status, "idle");

    agent.send("again", Delivery::Queue).unwrap();
    let final_state = agent.wait().await;
    assert_eq!(final_state.last_message.as_deref(), Some("reply to: again"));
}

#[tokio::test]
async fn killing_a_parent_kills_its_tree() {
    let h = harness(Arc::new(MockProvider::silent()));
    let parent = h.spawn(None, SpawnSpec::default()).unwrap();
    let child = h.spawn(Some(parent.id), SpawnSpec::default()).unwrap();
    let grandchild = h.spawn(Some(child.id), SpawnSpec::default()).unwrap();

    assert_eq!(h.descendants(parent.id).len(), 2);
    let killed = h.kill(parent.id).unwrap();
    assert_eq!(killed, 3);

    for handle in [&parent, &child, &grandchild] {
        handle.live.wait_idle().await;
        assert_eq!(handle.status(), AgentStatus::Killed);
    }
    assert_eq!(h.stats().killed, 3);
}

#[tokio::test]
async fn inspector_reports_live_state() {
    let h = harness(Arc::new(
        MockProvider::new(echo_reply)
            .with_chunk_chars(4)
            .with_delay(Duration::from_millis(20)),
    ));
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                title: Some("inspector test".into()),
                prompt: Some("take your time".into()),
                ..SpawnSpec::default()
            },
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;

    let state = agent.state().await;
    assert_eq!(state.title.as_deref(), Some("inspector test"));
    assert_eq!(state.status, "running");
    assert!(state.transcript_len >= 2);

    agent.wait().await;
    assert_eq!(h.state(agent.id).unwrap().status, "idle");
    assert!(h.list().iter().any(|s| s.agent_id == agent.id.wire()));
}
