//! A resumed agent starts from stored history, and a model switch can change
//! provider mid-session.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use destro_core::{
    ChatRequest, Delivery, Harness, HarnessConfig, Provider, ProviderItem, ProviderSet,
    ProviderStream, SpawnSpec,
};

/// Records every request and answers with a fixed word.
struct Recorder {
    name: &'static str,
    seen: Mutex<Vec<ChatRequest>>,
}

impl Provider for Recorder {
    fn name(&self) -> &'static str {
        self.name
    }
    fn stream(&self, req: ChatRequest, _cancel: CancellationToken) -> ProviderStream {
        self.seen.lock().push(req);
        futures_util::stream::iter(vec![
            Ok(ProviderItem::TextDelta(self.name.to_string())),
            Ok(ProviderItem::Done {
                finish_reason: "stop".into(),
            }),
        ])
        .boxed()
    }
}

fn recorder(name: &'static str) -> Arc<Recorder> {
    Arc::new(Recorder {
        name,
        seen: Mutex::new(vec![]),
    })
}

#[tokio::test]
async fn spawn_with_history_continues_the_conversation() {
    let rec = recorder("a");
    let mut cfg = HarnessConfig::new(rec.clone());
    cfg.system = "SYSTEM WITH AGENTS.md RULES".into();
    let h = Harness::new(cfg);
    let history: Vec<Value> = vec![
        json!({"role":"user","content":"earlier question"}),
        json!({"role":"assistant","content":"earlier answer"}),
    ];
    let agent = h
        .spawn(
            None,
            SpawnSpec {
                history,
                ..SpawnSpec::default()
            },
        )
        .unwrap();
    agent.send("follow-up", Delivery::Queue).unwrap();
    agent.wait().await;
    let seen = rec.seen.lock();
    let req = seen.last().expect("a request was made");
    assert_eq!(req.system, "SYSTEM WITH AGENTS.md RULES");
    let roles: Vec<&str> = req
        .messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    assert_eq!(req.messages[0]["content"], "earlier question");
    assert_eq!(req.messages[2]["content"], "follow-up");
}

#[tokio::test]
async fn set_model_with_a_route_switches_provider() {
    let a = recorder("a");
    let b = recorder("b");
    let mut map: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    map.insert("a".into(), a.clone());
    map.insert("b".into(), b.clone());
    let mut cfg = HarnessConfig::new(a.clone());
    cfg.providers = Some(ProviderSet::new("a", map));
    cfg.model = "m1".into();
    let h = Harness::new(cfg);
    let agent = h.spawn(None, SpawnSpec::default()).unwrap();

    agent.send("one", Delivery::Queue).unwrap();
    agent.wait().await;
    assert_eq!(a.seen.lock().len(), 1);
    assert!(b.seen.lock().is_empty());

    agent.set_model("b/m2");
    agent.send("two", Delivery::Queue).unwrap();
    agent.wait().await;
    assert_eq!(b.seen.lock().len(), 1, "second turn must go to provider b");
    assert_eq!(b.seen.lock()[0].model, "m2");
}
