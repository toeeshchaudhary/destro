//! Approvals, ask_user, compaction and nested instructions, driven through the
//! real agent loop with a scripted provider.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use destro_core::{
    ChatRequest, Decision, Delivery, Harness, HarnessConfig, HarnessEvent, Provider, ProviderItem,
    ProviderStream, SpawnSpec,
};
use destro_tools::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

/// Plays back one scripted response per request (then plain "ok") and records
/// every request it saw.
struct Script {
    replies: Mutex<VecDeque<Vec<ProviderItem>>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl Script {
    fn new(replies: Vec<Vec<ProviderItem>>) -> Arc<Script> {
        Arc::new(Script {
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(vec![]),
        })
    }
}

impl Provider for Script {
    fn name(&self) -> &'static str {
        "script"
    }
    fn stream(&self, req: ChatRequest, _c: CancellationToken) -> ProviderStream {
        self.seen.lock().push(req);
        let mut items = self
            .replies
            .lock()
            .pop_front()
            .unwrap_or_else(|| vec![ProviderItem::TextDelta("ok".into())]);
        items.push(ProviderItem::Done {
            finish_reason: "stop".into(),
        });
        futures_util::stream::iter(items.into_iter().map(Ok)).boxed()
    }
}

fn call(id: &str, name: &str, args: Value) -> ProviderItem {
    ProviderItem::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

/// A tool that counts runs and returns fixed text.
struct Probe {
    name: &'static str,
    out: String,
    runs: Arc<AtomicUsize>,
}

impl Tool for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "probe"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    fn run<'a>(&'a self, _: &'a ToolInvocation, _: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let out = self.out.clone();
        Box::pin(async move { ToolOutput::ok(out) })
    }
}

fn last_tool_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn rejected_tool_does_not_run_and_allowed_one_does() {
    let script = Script::new(vec![
        vec![call("c1", "edit", json!({"path": "a.txt"}))],
        vec![call("c2", "edit", json!({"path": "a.txt"}))],
    ]);
    let h = Harness::new(HarnessConfig::new(script.clone()));
    let runs = Arc::new(AtomicUsize::new(0));
    h.tools.register(Arc::new(Probe {
        name: "edit",
        out: "edited".into(),
        runs: runs.clone(),
    }));
    h.interact.attach();
    h.interact.set_approvals(true);
    let mut rx = h.subscribe();
    let agent = h.spawn(None, SpawnSpec::default()).unwrap();
    agent.send("go", Delivery::Queue).unwrap();

    let mut decisions = vec![Decision::Reject, Decision::Allow].into_iter();
    let mut asked = 0;
    let handle = h.clone();
    let waiter = tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let HarnessEvent::Approval {
                id, tool, summary, ..
            } = ev
            {
                assert_eq!(tool, "edit");
                assert_eq!(summary, "a.txt");
                asked += 1;
                match decisions.next() {
                    Some(d) => handle.interact.resolve_approval(id, d),
                    None => break,
                }
                if asked == 2 {
                    break;
                }
            }
        }
        asked
    });
    agent.wait().await;
    // the first call was rejected (tool untouched); the model retried and was allowed
    tokio::time::timeout(Duration::from_secs(5), async {
        while runs.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    agent.wait().await;
    assert_eq!(waiter.await.unwrap(), 2);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "only the allowed call ran");
    let seen = script.seen.lock();
    assert!(last_tool_text(&seen[1]).contains("rejected by the user"));
}

#[tokio::test]
async fn approvals_off_runs_everything_without_asking() {
    let script = Script::new(vec![vec![call("c1", "pwsh", json!({"command": "x"}))]]);
    let h = Harness::new(HarnessConfig::new(script));
    let runs = Arc::new(AtomicUsize::new(0));
    h.tools.register(Arc::new(Probe {
        name: "pwsh",
        out: "done".into(),
        runs: runs.clone(),
    }));
    h.interact.attach();
    let agent = h.spawn(None, SpawnSpec::default()).unwrap();
    agent.send("go", Delivery::Queue).unwrap();
    agent.wait().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ask_user_round_trips_and_fails_fast_when_nobody_is_attached() {
    // not attached: immediate error, no hang
    let script = Script::new(vec![vec![call(
        "q",
        "ask_user",
        json!({"question": "colour?"}),
    )]]);
    let h = Harness::new(HarnessConfig::new(script.clone()));
    let agent = h.spawn(None, SpawnSpec::default()).unwrap();
    agent.send("go", Delivery::Queue).unwrap();
    agent.wait().await;
    assert!(last_tool_text(&script.seen.lock()[1]).contains("no interactive user"));

    // attached: the question reaches the bus and the answer reaches the model
    let script = Script::new(vec![vec![call(
        "q",
        "ask_user",
        json!({"question": "colour?", "options": ["red", "blue"]}),
    )]]);
    let h = Harness::new(HarnessConfig::new(script.clone()));
    h.interact.attach();
    let mut rx = h.subscribe();
    let agent = h.spawn(None, SpawnSpec::default()).unwrap();
    agent.send("go", Delivery::Queue).unwrap();
    let handle = h.clone();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let HarnessEvent::Question {
                id,
                question,
                options,
                ..
            } = ev
            {
                assert_eq!(question, "colour?");
                assert_eq!(options, ["red", "blue"]);
                handle.interact.answer(id, Some("blue".into()));
                break;
            }
        }
    });
    agent.wait().await;
    assert!(last_tool_text(&script.seen.lock()[1]).contains("The user answered: blue"));
}

#[tokio::test]
async fn compaction_replaces_old_history_with_a_summary() {
    // first request is the summariser; later ones are normal turns
    let script = Script::new(vec![vec![ProviderItem::TextDelta(
        "SUMMARY OF EARLIER WORK".into(),
    )]]);
    let h = Harness::new(HarnessConfig::new(script.clone()));
    let mut history: Vec<Value> = vec![];
    for i in 0..6 {
        history.push(json!({"role":"user","content":format!("question {i}")}));
        history.push(json!({"role":"assistant","content":format!("answer {i}")}));
    }
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                history,
                ..SpawnSpec::default()
            },
        )
        .unwrap();
    agent.compact();
    tokio::time::sleep(Duration::from_millis(300)).await;
    agent.send("next", Delivery::Queue).unwrap();
    agent.wait().await;

    let seen = script.seen.lock();
    assert!(
        seen[0].system.contains("compress"),
        "first call is the summariser"
    );
    assert!(seen[0].tools.is_empty());
    let req = &seen[1];
    let first = req.messages[0]["content"].as_str().unwrap();
    assert!(first.starts_with("[Summary of the earlier conversation"));
    assert!(first.contains("SUMMARY OF EARLIER WORK"));
    assert!(
        req.messages.len() < 14,
        "history shrank: {}",
        req.messages.len()
    );
    assert_eq!(req.messages.last().unwrap()["content"], "next");
    // recent turns survive verbatim
    assert!(req.messages.iter().any(|m| m["content"] == "answer 5"));
    assert!(!req.messages.iter().any(|m| m["content"] == "question 0"));
}

#[tokio::test]
async fn auto_compaction_triggers_past_the_threshold() {
    let script = Script::new(vec![
        // turn 1 answers and reports a huge context
        vec![
            ProviderItem::TextDelta("first".into()),
            ProviderItem::Usage(destro_proto::event::Usage {
                input_tokens: 500,
                ..Default::default()
            }),
        ],
        vec![ProviderItem::TextDelta("SUM".into())], // summariser
    ]);
    let mut cfg = HarnessConfig::new(script.clone());
    cfg.compact_at = 100;
    let h = Harness::new(cfg);
    let mut history: Vec<Value> = vec![];
    for i in 0..6 {
        history.push(json!({"role":"user","content":format!("q{i}")}));
        history.push(json!({"role":"assistant","content":format!("a{i}")}));
    }
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                history,
                ..SpawnSpec::default()
            },
        )
        .unwrap();
    agent.send("one", Delivery::Queue).unwrap();
    agent.wait().await;
    agent.send("two", Delivery::Queue).unwrap();
    agent.wait().await;
    let seen = script.seen.lock();
    assert!(
        seen.iter().any(|r| r.system.contains("compress")),
        "summariser ran"
    );
}

#[tokio::test]
async fn nested_instructions_ride_along_with_the_first_read_in_a_subfolder() {
    let dir = std::env::temp_dir().join(format!("destro-nested-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(dir.join("pkg").join("AGENTS.md"), "PKG RULES").unwrap();
    let script = Script::new(vec![
        vec![call("r1", "read", json!({"path": "pkg/lib.rs"}))],
        vec![call("r2", "read", json!({"path": "pkg/other.rs"}))],
    ]);
    let mut cfg = HarnessConfig::new(script.clone());
    cfg.cwd = dir.clone();
    let h = Harness::new(cfg);
    let runs = Arc::new(AtomicUsize::new(0));
    h.tools.register(Arc::new(Probe {
        name: "read",
        out: "file body".into(),
        runs,
    }));
    let agent = h.spawn(None, SpawnSpec::default()).unwrap();
    agent.send("go", Delivery::Queue).unwrap();
    agent.wait().await;
    let seen = script.seen.lock();
    let first = last_tool_text(&seen[1]);
    assert!(
        first.contains("PKG RULES") && first.contains("file body"),
        "{first}"
    );
    let second = last_tool_text(&seen[2]);
    assert!(
        !second.contains("PKG RULES"),
        "only injected once: {second}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tool that panics.
struct Boom;

impl Tool for Boom {
    fn name(&self) -> &str {
        "boom"
    }
    fn description(&self) -> &str {
        "panics"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    fn run<'a>(&'a self, _: &'a ToolInvocation, _: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { panic!("byte index 12 is not a char boundary") })
    }
}

#[tokio::test]
async fn a_panicking_tool_is_reported_and_the_agent_survives() {
    let script = Script::new(vec![vec![call("b1", "boom", json!({}))]]);
    let h = Harness::new(HarnessConfig::new(script.clone()));
    h.tools.register(Arc::new(Boom));
    let agent = h.spawn(None, SpawnSpec::default()).unwrap();
    agent.send("go", Delivery::Queue).unwrap();
    agent.wait().await;
    let result = {
        let seen = script.seen.lock();
        last_tool_text(&seen[1])
    };
    assert!(
        result.contains("crashed") && result.contains("not a char boundary"),
        "{result}"
    );
    // still alive: it answers the next message
    agent.send("again", Delivery::Queue).unwrap();
    agent.wait().await;
    assert_eq!(agent.status(), destro_core::AgentStatus::Idle);
}
