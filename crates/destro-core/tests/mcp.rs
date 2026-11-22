//! End-to-end MCP: a tiny Python stdio server, driven through `McpHub`.
//! Skipped (with a note) when no `python` is on PATH.

use std::sync::Arc;
use std::time::Duration;

use destro_core::mcp::{McpHub, McpState};
use destro_tools::{Registry, ToolContext, ToolInvocation};

const SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m:
        continue
    if m['method'] == 'initialize':
        r = {'protocolVersion': '2025-06-18', 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'stub', 'version': '1'}}
    elif m['method'] == 'tools/list':
        r = {'tools': [{'name': 'shout', 'description': 'upper-case text',
                        'inputSchema': {'type': 'object', 'properties': {'text': {'type': 'string'}}}}]}
    elif m['method'] == 'tools/call':
        t = m['params']['arguments'].get('text', '')
        r = {'content': [{'type': 'text', 'text': t.upper()}]}
    else:
        r = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': m['id'], 'result': r}), flush=True)
"#;

fn python() -> Option<&'static str> {
    ["python", "python3"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

#[tokio::test]
async fn stdio_server_registers_and_runs_tools() {
    let Some(py) = python() else {
        eprintln!("skipping: no python");
        return;
    };
    let dir = std::env::temp_dir().join(format!("destro-mcp-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("server.py"), SERVER).unwrap();
    // user-scope config (project scope would wait for approval)
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let script = dir.join("server.py").to_string_lossy().replace('\\', "/");
    std::fs::write(
        home.join("mcp.json"),
        format!(r#"{{"mcpServers":{{"stub":{{"command":"{py}","args":["{script}"]}}}}}}"#),
    )
    .unwrap();
    // SAFETY: single test in this binary mutates the environment.
    unsafe { std::env::set_var("DESTRO_HOME", &home) };

    let registry = Arc::new(Registry::default());
    let hub = McpHub::start(&dir, registry.clone(), tokio::runtime::Handle::current());

    let mut state = McpState::Connecting;
    for _ in 0..100 {
        state = hub.statuses()[0].state.clone();
        if state != McpState::Connecting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(state, McpState::Ready, "{:?}", hub.statuses());
    assert_eq!(registry.names(), vec!["mcp__stub__shout".to_string()]);

    let tool = registry.get("mcp__stub__shout").unwrap();
    let out = tool
        .run(
            &ToolInvocation {
                call_id: "1".into(),
                arguments: serde_json::json!({"text": "hello"}),
            },
            &ToolContext {
                cwd: dir.clone(),
                agent_id: "a".into(),
            },
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "HELLO");

    hub.set_enabled("stub", false);
    assert!(registry.names().is_empty());
    assert_eq!(hub.statuses()[0].state, McpState::Disabled);
    let _ = std::fs::remove_dir_all(&dir);
}
