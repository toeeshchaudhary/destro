//! End-to-end MCP: a tiny Python stdio server, driven through `McpHub`.
//! Skipped (with a note) when no `python` is on PATH.

use std::sync::{Arc, Mutex};
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

/// The tests point `DESTRO_HOME` at their own folder; the hub reads it once, at
/// start, so serialising set-and-start is enough.
static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: serialised by ENV_LOCK; the hub reads the variable synchronously in `start`.
    unsafe { std::env::set_var("DESTRO_HOME", &home) };
    let registry = Arc::new(Registry::default());
    let hub = McpHub::start(&dir, registry.clone(), tokio::runtime::Handle::current());
    drop(guard);

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

// ----- resources, prompts, live refresh --------------------------------------------------

const RICH_SERVER: &str = r#"
import sys, json
extra = False
def send(o): print(json.dumps(o), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m:
        continue
    meth = m['method']
    if meth == 'initialize':
        r = {'protocolVersion': '2025-06-18', 'serverInfo': {'name': 'rich', 'version': '1'},
             'capabilities': {'tools': {'listChanged': True}, 'resources': {}, 'prompts': {}}}
    elif meth == 'tools/list':
        tools = [{'name': 'grow', 'description': 'adds a tool', 'inputSchema': {'type': 'object'}}]
        if extra:
            tools.append({'name': 'bonus', 'description': 'appeared later', 'inputSchema': {'type': 'object'}})
        r = {'tools': tools}
    elif meth == 'tools/call':
        extra = True
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': {'content': [{'type': 'text', 'text': 'grown'}]}})
        send({'jsonrpc': '2.0', 'method': 'notifications/tools/list_changed'})
        continue
    elif meth == 'resources/list':
        r = {'resources': [{'uri': 'mem://notes', 'name': 'notes', 'mimeType': 'text/plain', 'description': 'my notes'}]}
    elif meth == 'resources/templates/list':
        r = {'resourceTemplates': [{'uriTemplate': 'mem://doc/{id}', 'name': 'doc'}]}
    elif meth == 'resources/read':
        r = {'contents': [{'uri': m['params']['uri'], 'text': 'contents of ' + m['params']['uri']}]}
    elif meth == 'prompts/list':
        r = {'prompts': [{'name': 'review', 'description': 'Review a file',
                          'arguments': [{'name': 'file', 'description': 'path', 'required': True}]}]}
    elif meth == 'prompts/get':
        f = m['params']['arguments'].get('file', '?')
        r = {'messages': [{'role': 'user', 'content': {'type': 'text', 'text': 'Please review ' + f}}]}
    else:
        r = {}
    send({'jsonrpc': '2.0', 'id': m['id'], 'result': r})
"#;

async fn wait_ready(hub: &McpHub) {
    for _ in 0..100 {
        if hub.statuses()[0].state != McpState::Connecting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn cx() -> ToolContext {
    ToolContext {
        cwd: std::env::temp_dir(),
        agent_id: "a".into(),
    }
}

fn inv(args: serde_json::Value) -> ToolInvocation {
    ToolInvocation {
        call_id: "1".into(),
        arguments: args,
    }
}

#[tokio::test]
async fn resources_prompts_and_live_tool_refresh() {
    let Some(py) = python() else {
        eprintln!("skipping: no python");
        return;
    };
    let dir = std::env::temp_dir().join(format!("destro-mcp-rich-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(dir.join("rich.py"), RICH_SERVER).unwrap();
    let script = dir.join("rich.py").to_string_lossy().replace('\\', "/");
    std::fs::write(
        home.join("mcp.json"),
        format!(r#"{{"mcpServers":{{"rich":{{"command":"{py}","args":["{script}"]}}}}}}"#),
    )
    .unwrap();
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: serialised by ENV_LOCK; the hub reads the variable synchronously in `start`.
    unsafe { std::env::set_var("DESTRO_HOME", &home) };
    let registry = Arc::new(Registry::default());
    let hub = McpHub::start(&dir, registry.clone(), tokio::runtime::Handle::current());
    drop(guard);
    wait_ready(&hub).await;
    let st = hub.statuses().remove(0);
    assert_eq!(st.state, McpState::Ready, "{st:?}");
    assert!(st.has_resources);
    assert_eq!(st.prompts, 1);
    assert_eq!(
        registry.names(),
        vec![
            "mcp__rich__grow".to_string(),
            "mcp__rich__list_resources".to_string(),
            "mcp__rich__read_resource".to_string(),
        ]
    );

    // resources
    let list = registry
        .get("mcp__rich__list_resources")
        .unwrap()
        .run(&inv(serde_json::json!({})), &cx())
        .await;
    assert!(
        list.content
            .contains("mem://notes — notes (text/plain): my notes"),
        "{}",
        list.content
    );
    assert!(list.content.contains("mem://doc/{id}") && list.content.contains("[template]"));
    let read = registry
        .get("mcp__rich__read_resource")
        .unwrap()
        .run(&inv(serde_json::json!({"uri": "mem://notes"})), &cx())
        .await;
    assert_eq!(read.content, "contents of mem://notes");

    // prompts
    let prompts = hub.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].arguments[0].name, "file");
    let mut args = serde_json::Map::new();
    args.insert("file".into(), "src/lib.rs".into());
    let text = hub.get_prompt("rich", "review", args).await.unwrap();
    assert_eq!(text, "user: Please review src/lib.rs");
    assert!(
        hub.get_prompt("nope", "review", Default::default())
            .await
            .is_err()
    );

    // live refresh: calling `grow` makes the server announce a new tool
    let out = registry
        .get("mcp__rich__grow")
        .unwrap()
        .run(&inv(serde_json::json!({})), &cx())
        .await;
    assert_eq!(out.content, "grown");
    let mut appeared = false;
    for _ in 0..60 {
        if registry.get("mcp__rich__bonus").is_some() {
            appeared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        appeared,
        "list_changed must refresh the tool list: {:?}",
        registry.names()
    );
    assert_eq!(hub.statuses()[0].tools.len(), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- legacy HTTP+SSE -------------------------------------------------------------------

const SSE_SERVER: &str = r#"
import json, queue
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
q = queue.Queue()
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        self.wfile.write(b'event: endpoint\ndata: /messages?session=7\n\n'); self.wfile.flush()
        while True:
            try:
                msg = q.get(timeout=30)
            except queue.Empty:
                break
            self.wfile.write(('event: message\ndata: ' + json.dumps(msg) + '\n\n').encode()); self.wfile.flush()
    def do_POST(self):
        n = int(self.headers.get('Content-Length', 0))
        m = json.loads(self.rfile.read(n))
        self.send_response(202); self.send_header('Content-Length', '0'); self.end_headers()
        if 'id' not in m:
            return
        if m['method'] == 'initialize':
            r = {'protocolVersion': '2025-06-18', 'capabilities': {'tools': {}}, 'serverInfo': {'name': 's', 'version': '1'}}
        elif m['method'] == 'tools/list':
            r = {'tools': [{'name': 'echo', 'description': 'echo', 'inputSchema': {'type': 'object'}}]}
        elif m['method'] == 'tools/call':
            r = {'content': [{'type': 'text', 'text': 'sse:' + m['params']['arguments'].get('t', '')}]}
        else:
            r = {}
        q.put({'jsonrpc': '2.0', 'id': m['id'], 'result': r})
srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
print(srv.server_address[1], flush=True)
srv.serve_forever()
"#;

#[tokio::test]
async fn legacy_sse_transport_round_trips() {
    let Some(py) = python() else {
        eprintln!("skipping: no python");
        return;
    };
    let dir = std::env::temp_dir().join(format!("destro-mcp-sse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(dir.join("sse.py"), SSE_SERVER).unwrap();
    let mut child = std::process::Command::new(py)
        .arg(dir.join("sse.py"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut port = String::new();
    {
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut port)
            .unwrap();
    }
    let port = port.trim().to_string();
    std::fs::write(
        home.join("mcp.json"),
        format!(
            r#"{{"mcpServers":{{"old":{{"type":"sse","url":"http://127.0.0.1:{port}/sse"}}}}}}"#
        ),
    )
    .unwrap();
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: serialised by ENV_LOCK; the hub reads the variable synchronously in `start`.
    unsafe { std::env::set_var("DESTRO_HOME", &home) };
    let registry = Arc::new(Registry::default());
    let hub = McpHub::start(&dir, registry.clone(), tokio::runtime::Handle::current());
    drop(guard);
    wait_ready(&hub).await;
    let st = hub.statuses().remove(0);
    assert_eq!(st.state, McpState::Ready, "{st:?}");
    let out = registry
        .get("mcp__old__echo")
        .expect("tool registered over SSE")
        .run(&inv(serde_json::json!({"t": "hi"})), &cx())
        .await;
    assert_eq!(out.content, "sse:hi");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
