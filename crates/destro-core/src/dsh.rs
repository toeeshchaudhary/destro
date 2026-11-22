//! Import chats from DeepSeek Harness (`dsh`).
//!
//! dsh keeps one zstd-compressed JSONL log per session under
//! `$DSH_HOME/sessions/<workspace>/<session>/session.v4.jsonl.zstd` (`$DSH_HOME`
//! defaults to `~/.dsh`). [`scan`] lists the *root* sessions grouped by
//! workspace, each carrying the subagent sessions it spawned, and
//! [`import_session`] converts one chat (and its subagents) into Destro session
//! files the TUI can open and the agent can continue.
//!
//! What survives: human prompts, assistant text and reasoning, tool calls and
//! results, titles, token usage, the original timestamps, and the subagent tree
//! (each subagent becomes a child session of its parent). Images become
//! `[image: <path>]` pointing at dsh's attachment store. dsh bookkeeping
//! (runtime-context snapshots, skill catalogues, sandbox and approval records,
//! jobs) has no Destro equivalent and is dropped. A log cut off mid-write
//! imports whatever decoded cleanly.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::context::home_dir;
use crate::store::{self, SessionWriter, blocks_text};

/// A subagent session that ran under a root chat.
#[derive(Clone, Debug)]
pub struct DshChild {
    /// dsh session id without the `session-` prefix.
    pub id: String,
    /// Id of the session that spawned it (normalised the same way).
    pub parent: String,
    pub file: PathBuf,
    pub title: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug)]
pub struct DshSession {
    /// dsh session id without the `session-` prefix.
    pub id: String,
    pub file: PathBuf,
    pub bytes: u64,
    pub title: String,
    pub created_ms: u64,
    /// Subagent sessions under this chat, direct and nested.
    pub children: Vec<DshChild>,
}

#[derive(Clone, Debug)]
pub struct DshWorkspace {
    /// The workspace directory the chats ran in.
    pub path: String,
    pub sessions: Vec<DshSession>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Imported {
    New,
    AlreadyThere,
}

pub fn dsh_home() -> Option<PathBuf> {
    match std::env::var_os("DSH_HOME") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => home_dir().map(|h| h.join(".dsh")),
    }
}

/// `session-<uuid>` and `<uuid>` name the same session in different fields.
fn norm(id: &str) -> String {
    id.strip_prefix("session-").unwrap_or(id).to_string()
}

/// Id of an imported session.
pub fn imported_id(dsh_id: &str) -> String {
    format!("dsh-{}", norm(dsh_id))
}

/// Decompress a session log, keeping whatever decoded before any error.
///
/// dsh appends one zstd *frame* per flush, so a log is many concatenated frames
/// (thousands, for a long chat) and each must be decoded in turn. `max` stops
/// early once that many bytes are in hand.
fn decode(file: &Path, max: Option<usize>) -> Vec<u8> {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(file) else {
        return vec![];
    };
    let mut src = std::io::BufReader::with_capacity(64 * 1024, f);
    let mut out = Vec::new();
    // one decoder for every frame: allocating its window afresh thousands of
    // times dominated the import time
    let mut frames = ruzstd::decoding::FrameDecoder::new();
    loop {
        if max.is_some_and(|m| out.len() >= m) {
            break;
        }
        match src.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(mut dec) =
            ruzstd::decoding::StreamingDecoder::new_with_decoder(&mut src, &mut frames)
        else {
            break;
        };
        match max {
            None => {
                if dec.read_to_end(&mut out).is_err() {
                    break;
                }
            }
            Some(m) => {
                let mut chunk = [0u8; 16 * 1024];
                while out.len() < m {
                    match dec.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => out.extend_from_slice(&chunk[..n]),
                        Err(_) => return out,
                    }
                }
            }
        }
    }
    out
}

fn rows(file: &Path) -> Vec<Value> {
    String::from_utf8_lossy(&decode(file, None))
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn header(file: &Path) -> Option<Value> {
    let buf = decode(file, Some(64 * 1024));
    let line = buf.split(|&b| b == b'\n').next()?;
    let v: Value = serde_json::from_slice(line).ok()?;
    (v["type"] == "session").then_some(v)
}

/// A readable title without decoding the whole log: the recorded title, a
/// subagent's label, else the first human prompt.
fn peek_title(file: &Path) -> String {
    let buf = decode(file, Some(256 * 1024));
    let text = String::from_utf8_lossy(&buf);
    let mut label = String::new();
    let mut first_user = String::new();
    for line in text.split('\n') {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // includes the cut-off last line
        };
        match v["type"].as_str() {
            Some("session/title") => {
                if let Some(t) = v["data"]["title"].as_str() {
                    return t.to_string();
                }
            }
            Some("subagent/descriptor") => {
                if let Some(l) = v["data"]["label"].as_str() {
                    label = l.to_string();
                }
            }
            Some("user/message")
                if first_user.is_empty() && v["data"]["source"]["kind"] == "user" =>
            {
                first_user = blocks_text(&v["data"]["content"]);
            }
            _ => {}
        }
    }
    if !label.is_empty() {
        return label;
    }
    first_user
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(60)
        .collect()
}

/// Root sessions under `home`, grouped by workspace (newest first), each with
/// the subagent sessions beneath it.
pub fn scan(home: &Path) -> Vec<DshWorkspace> {
    let Ok(wss) = std::fs::read_dir(home.join("sessions")) else {
        return vec![];
    };
    let mut roots: HashMap<String, (String, DshSession)> = HashMap::new(); // id -> (cwd, session)
    let mut subs: Vec<DshChild> = vec![];
    for ws in wss.flatten() {
        let Ok(sess) = std::fs::read_dir(ws.path()) else {
            continue;
        };
        for s in sess.flatten() {
            let file = s.path().join("session.v4.jsonl.zstd");
            if !file.is_file() {
                continue;
            }
            let Some(h) = header(&file) else { continue };
            let id = norm(h["id"].as_str().unwrap_or(""));
            if id.is_empty() {
                continue;
            }
            let created_ms = h["createdAt"].as_u64().unwrap_or(0);
            if h["origin"] == "subagent" || h["parentSession"].is_string() {
                subs.push(DshChild {
                    id,
                    parent: norm(h["parentSession"].as_str().unwrap_or("")),
                    title: peek_title(&file),
                    file,
                    created_ms,
                });
                continue;
            }
            let cwd = h["cwd"].as_str().unwrap_or("").to_string();
            if cwd.is_empty() {
                continue;
            }
            roots.insert(
                id.clone(),
                (
                    cwd,
                    DshSession {
                        id,
                        bytes: file.metadata().map(|m| m.len()).unwrap_or(0),
                        title: peek_title(&file),
                        file,
                        created_ms,
                        children: vec![],
                    },
                ),
            );
        }
    }
    // hang every subagent under the root its parent chain ends at
    let parent_of: HashMap<String, String> = subs
        .iter()
        .map(|c| (c.id.clone(), c.parent.clone()))
        .collect();
    for c in subs {
        let mut at = c.parent.clone();
        for _ in 0..16 {
            if roots.contains_key(&at) {
                break;
            }
            match parent_of.get(&at) {
                Some(p) => at = p.clone(),
                None => break,
            }
        }
        if let Some((_, root)) = roots.get_mut(&at) {
            root.children.push(c);
        }
    }
    let mut by_ws: std::collections::BTreeMap<String, Vec<DshSession>> = Default::default();
    for (_, (cwd, mut s)) in roots {
        s.children.sort_by_key(|c| c.created_ms);
        by_ws.entry(cwd).or_default().push(s);
    }
    let mut out: Vec<DshWorkspace> = by_ws
        .into_iter()
        .map(|(path, mut sessions)| {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
            DshWorkspace { path, sessions }
        })
        .collect();
    out.sort_by_key(|w| std::cmp::Reverse(w.sessions.first().map(|s| s.created_ms)));
    out
}

/// Text of dsh content blocks; images point at dsh's attachment store.
fn dsh_text(content: &Value, home: Option<&Path>) -> String {
    match content {
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_string),
                Some("image") => Some(image_note(b, home)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => blocks_text(other),
    }
}

fn image_note(block: &Value, home: Option<&Path>) -> String {
    let id = block["attachment"]["attachmentId"].as_str().unwrap_or("");
    let hash = id.strip_prefix("sha256:").unwrap_or(id);
    if hash.len() >= 2
        && let Some(h) = home
    {
        let p = h
            .join("attachments")
            .join("v1")
            .join("objects")
            .join(&hash[..2])
            .join(hash);
        if p.exists() {
            return format!("[image: {}]", p.display());
        }
    }
    "[image]".to_string()
}

/// Convert dsh rows to Destro events. Returns `(events, title, model, last_ms)`.
pub fn convert(rows: &[Value], home: Option<&Path>) -> (Vec<Value>, String, String, u64) {
    let mut out = vec![];
    let mut title = String::new();
    let mut label = String::new();
    let mut model = String::new();
    let mut last_ms = 0u64;
    for r in rows {
        let d = &r["data"];
        if let Some(t) = r["time"].as_u64() {
            last_ms = last_ms.max(t);
        }
        match r["type"].as_str().unwrap_or("") {
            "subagent/descriptor" => {
                if let Some(l) = d["label"].as_str() {
                    label = l.to_string();
                }
            }
            "turn/start" => out.push(json!({"type":"turn/start","data":{"turn":d["turn"]}})),
            "turn/end" => {
                out.push(json!({"type":"turn/end","data":{"turn":d["turn"],"reason":d["reason"]}}))
            }
            "user/message" => {
                if d["source"]["kind"].as_str().unwrap_or("user") == "user" {
                    let text = dsh_text(&d["content"], home);
                    if !text.is_empty() {
                        out.push(json!({"type":"user/message","data":{
                            "content":[{"type":"text","text":text}],"source":{"kind":"user"}}}));
                    }
                }
            }
            "assistant/message" => {
                let msg = &d["message"];
                if model.is_empty()
                    && let Some(m) = msg["source"]["model"].as_str()
                {
                    model = m.to_string();
                }
                let content: Vec<Value> = msg["content"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|b| matches!(b["type"].as_str(), Some("text" | "reasoning")))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let mut data = json!({
                    "message": {"role":"assistant","content": content},
                    "interrupted": d["interrupted"].as_bool().unwrap_or(false),
                });
                if d["usage"].is_object() {
                    data["usage"] = d["usage"].clone();
                }
                out.push(json!({"type":"assistant/message","data":data}));
            }
            "tool/call" => out.push(json!({"type":"tool/call","data":{
                "callId":d["callId"],"name":d["name"],"arguments":d["arguments"]}})),
            "tool/result" => {
                let m = &d["message"];
                out.push(json!({"type":"tool/result","data":{"message":{
                    "role":"tool",
                    "toolCallId": m["toolCallId"],
                    "name": m["name"],
                    "isError": m["isError"].as_bool().unwrap_or(false),
                    "content":[{"type":"text","text":dsh_text(&m["content"], home)}],
                }}}));
            }
            "session/title" => {
                if let Some(t) = d["title"].as_str() {
                    title = t.to_string();
                    out.push(json!({"type":"session/title","data":{"title":t}}));
                }
            }
            "request/context" if d["contextWindow"].is_u64() => {
                out.push(
                    json!({"type":"request/context","data":{"contextWindow":d["contextWindow"]}}),
                );
            }
            _ => {}
        }
    }
    if title.is_empty() && !label.is_empty() {
        title = label.clone();
        out.insert(0, json!({"type":"session/title","data":{"title":label}}));
    }
    (out, title, model, last_ms)
}

/// Write converted events as a Destro session file; atomic via a `.part` file.
fn write_session(
    path: &Path,
    cwd: &str,
    id: &str,
    events: &[Value],
    extra: Value,
) -> Result<(), String> {
    let tmp = path.with_extension("jsonl.part");
    let _ = std::fs::remove_file(&tmp);
    {
        // dsh model names are not routes in this install; resume uses the
        // workspace's current model instead
        let mut w = SessionWriter::open_at_with(&tmp, cwd, id, "", "dsh", extra)
            .map_err(|e| e.to_string())?;
        for e in events {
            w.append(e);
        }
    }
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Import one dsh chat (and its subagents) into the Destro store under `cwd`.
pub fn import_session(s: &DshSession, cwd: &str, force: bool) -> Result<Imported, String> {
    let dir = store::sessions_dir(cwd).ok_or("no data directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = imported_id(&s.id);
    let path = dir.join(format!("{id}.jsonl"));
    if path.exists() && !force {
        return Ok(Imported::AlreadyThere);
    }
    let home = dsh_home();
    let all = rows(&s.file);
    if all.is_empty() {
        return Err(format!("{}: nothing readable", s.file.display()));
    }
    let (events, _title, _model, last_ms) = convert(&all, home.as_deref());
    if !events.iter().any(|e| e["type"] == "user/message") {
        return Err("no conversation in this session".into());
    }
    let mut extra = json!({"importedFrom": "dsh", "dshId": s.id});
    if last_ms > 0 {
        extra["updated"] = json!(last_ms);
    }
    if s.created_ms > 0 {
        extra["created"] = json!(s.created_ms);
    }
    write_session(&path, cwd, &id, &events, extra)?;

    for c in &s.children {
        let child_rows = rows(&c.file);
        let (ev, _, _, last) = convert(&child_rows, home.as_deref());
        if ev.is_empty() {
            continue;
        }
        let child_id = imported_id(&c.id);
        let mut extra = json!({
            "parent": imported_id(&c.parent),
            "root": id,
            "importedFrom": "dsh",
        });
        if last > 0 {
            extra["updated"] = json!(last);
        }
        if c.created_ms > 0 {
            extra["created"] = json!(c.created_ms);
        }
        // a failed child must not undo the chat that did import
        let _ = write_session(
            &store::child_path(&dir, &id, &child_id),
            cwd,
            &child_id,
            &ev,
            extra,
        );
    }
    Ok(Imported::New)
}

/// Import many chats on all cores. Results come back in input order.
pub fn import_many(jobs: &[(DshSession, String)], force: bool) -> Vec<Result<Imported, String>> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 8)
        .min(jobs.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<Option<Result<Imported, String>>>> =
        std::sync::Mutex::new(vec![None; jobs.len()]);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((s, cwd)) = jobs.get(i) else { break };
                    let r = import_session(s, cwd, force);
                    results.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err("not run".into())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_keeps_conversation_and_drops_bookkeeping() {
        let rows = vec![
            json!({"type":"session","version":4}),
            json!({"type":"user/message","time":10,"data":{"content":[{"type":"text","text":"snapshot"}],"source":{"kind":"runtime-context"}}}),
            json!({"type":"user/message","time":20,"data":{"content":[{"type":"text","text":"hello"}],"source":{"kind":"user"}}}),
            json!({"type":"assistant/message","time":30,"data":{"message":{"content":[
                {"type":"reasoning","text":"think"},{"type":"text","text":"hi"},
                {"type":"tool-call","id":"c","name":"read","arguments":"{}"}],
                "source":{"model":"m1"}},"usage":{"inputTokens":5}}}),
            json!({"type":"tool/call","data":{"callId":"c","name":"read","arguments":"{}"}}),
            json!({"type":"tool/result","data":{"message":{"toolCallId":"c","content":[{"type":"image"},{"type":"text","text":"out"}]}}}),
            json!({"type":"job/registered","data":{}}),
        ];
        let (ev, _, model, last) = convert(&rows, None);
        let types: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "user/message",
                "assistant/message",
                "tool/call",
                "tool/result"
            ]
        );
        assert_eq!(model, "m1");
        assert_eq!(last, 30);
        let blocks = ev[1]["data"]["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "tool-call block must be dropped");
        assert_eq!(ev[1]["data"]["usage"]["inputTokens"], 5);
        assert_eq!(
            ev[3]["data"]["message"]["content"][0]["text"],
            "[image]\nout"
        );
        let msgs = store::replay_messages(&ev);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "c");
    }

    #[test]
    fn subagent_label_becomes_the_title() {
        let rows = vec![
            json!({"type":"subagent/descriptor","data":{"label":"Implement destro-tools crate"}}),
            json!({"type":"user/message","data":{"content":[{"type":"text","text":"do it"}],"source":{"kind":"user"}}}),
        ];
        let (ev, title, _, _) = convert(&rows, None);
        assert_eq!(title, "Implement destro-tools crate");
        assert_eq!(ev[0]["type"], "session/title");
    }

    #[test]
    fn images_point_at_the_attachment_store_when_it_exists() {
        let home = std::env::temp_dir().join(format!("destro-dsh-img-{}", std::process::id()));
        let obj = home.join("attachments/v1/objects/ab");
        std::fs::create_dir_all(&obj).unwrap();
        std::fs::write(obj.join("abcdef"), b"png").unwrap();
        let block = json!({"type":"image","attachment":{"attachmentId":"sha256:abcdef"}});
        let note = image_note(&block, Some(&home));
        assert!(
            note.starts_with("[image: ") && note.contains("abcdef"),
            "{note}"
        );
        let missing = json!({"type":"image","attachment":{"attachmentId":"sha256:zzzz"}});
        assert_eq!(image_note(&missing, Some(&home)), "[image]");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn ids_normalise() {
        assert_eq!(norm("session-abc"), "abc");
        assert_eq!(norm("abc"), "abc");
        assert_eq!(imported_id("session-abc"), "dsh-abc");
    }
}
