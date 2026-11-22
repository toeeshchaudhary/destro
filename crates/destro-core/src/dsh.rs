//! Import chats from DeepSeek Harness (`dsh`).
//!
//! dsh keeps one zstd-compressed JSONL log per session under
//! `$DSH_HOME/sessions/<workspace>/<session>/session.v4.jsonl.zstd` (`$DSH_HOME`
//! defaults to `~/.dsh`). [`scan`] lists the *root* sessions grouped by
//! workspace (subagent sessions are folded into their parent's tool calls, so
//! they are not separate chats), and [`import_session`] converts one into a
//! Destro session file that the TUI can open and the agent can continue.
//!
//! Only the conversation survives the conversion: human prompts, assistant text
//! and reasoning, tool calls and their results, titles, token usage. dsh's
//! bookkeeping (runtime-context snapshots, skill catalogues, sandbox and
//! approval records, jobs) has no Destro equivalent and is dropped. A log cut
//! off mid-write imports whatever decoded cleanly.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::context::home_dir;
use crate::store::{self, SessionWriter, blocks_text};

#[derive(Clone, Debug)]
pub struct DshSession {
    pub id: String,
    pub file: PathBuf,
    pub bytes: u64,
    pub title: String,
    pub created_ms: u64,
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
    loop {
        if max.is_some_and(|m| out.len() >= m) {
            break;
        }
        match src.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(_) => {}
        }
        let before = out.len();
        let Ok(mut dec) = ruzstd::decoding::StreamingDecoder::new(&mut src) else {
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
        if out.len() == before && max.is_none() {
            // an empty frame still consumed input; a stuck reader would not
            continue;
        }
    }
    out
}

fn read_all(file: &Path, limit: Option<usize>) -> Vec<u8> {
    decode(file, limit)
}

fn read_prefix(file: &Path, max: usize) -> Vec<u8> {
    decode(file, Some(max))
}

/// A readable title without decoding the whole log: the recorded title, else the
/// first human prompt.
fn peek_title(file: &Path) -> String {
    let buf = read_prefix(file, 256 * 1024);
    let text = String::from_utf8_lossy(&buf);
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
            Some("user/message")
                if first_user.is_empty() && v["data"]["source"]["kind"] == "user" =>
            {
                first_user = blocks_text(&v["data"]["content"]);
            }
            _ => {}
        }
    }
    first_user
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(60)
        .collect()
}

fn rows(file: &Path) -> Vec<Value> {
    String::from_utf8_lossy(&read_all(file, None))
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn header(file: &Path) -> Option<Value> {
    let buf = read_all(file, Some(1 << 20));
    let line = buf.split(|&b| b == b'\n').next()?;
    let v: Value = serde_json::from_slice(line).ok()?;
    (v["type"] == "session").then_some(v)
}

/// Root sessions under `home`, grouped by workspace, newest workspace first.
pub fn scan(home: &Path) -> Vec<DshWorkspace> {
    let mut by_ws: std::collections::BTreeMap<String, Vec<DshSession>> = Default::default();
    let Ok(wss) = std::fs::read_dir(home.join("sessions")) else {
        return vec![];
    };
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
            if h["origin"] == "subagent" || h["parentSession"].is_string() {
                continue;
            }
            let cwd = h["cwd"].as_str().unwrap_or("").to_string();
            if cwd.is_empty() {
                continue;
            }
            by_ws.entry(cwd).or_default().push(DshSession {
                id: h["id"].as_str().unwrap_or("").to_string(),
                bytes: file.metadata().map(|m| m.len()).unwrap_or(0),
                title: peek_title(&file),
                file,
                created_ms: h["createdAt"].as_u64().unwrap_or(0),
            });
        }
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

/// Convert dsh rows to Destro events. Returns `(events, title, model)`.
pub fn convert(rows: &[Value]) -> (Vec<Value>, String, String) {
    let mut out = vec![];
    let mut title = String::new();
    let mut model = String::new();
    for r in rows {
        let d = &r["data"];
        match r["type"].as_str().unwrap_or("") {
            "turn/start" => out.push(json!({"type":"turn/start","data":{"turn":d["turn"]}})),
            "turn/end" => {
                out.push(json!({"type":"turn/end","data":{"turn":d["turn"],"reason":d["reason"]}}))
            }
            "user/message" => {
                if d["source"]["kind"].as_str().unwrap_or("user") == "user" {
                    let text = blocks_text(&d["content"]);
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
                    "content":[{"type":"text","text":blocks_text(&m["content"])}],
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
    (out, title, model)
}

/// Import one dsh session into the Destro store under workspace `cwd`.
pub fn import_session(s: &DshSession, cwd: &str, force: bool) -> Result<Imported, String> {
    let dir = store::sessions_dir(cwd).ok_or("no data directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = format!("dsh-{}", s.id);
    let path = dir.join(format!("{id}.jsonl"));
    if path.exists() && !force {
        return Ok(Imported::AlreadyThere);
    }
    let all = rows(&s.file);
    if all.is_empty() {
        return Err(format!("{}: nothing readable", s.file.display()));
    }
    let (events, _title, model) = convert(&all);
    if !events.iter().any(|e| e["type"] == "user/message") {
        return Err("no conversation in this session".into());
    }
    let _ = std::fs::remove_file(&path);
    // Write beside the final name first so a half-written import never shows up.
    let tmp = dir.join(format!("{id}.jsonl.part"));
    let _ = std::fs::remove_file(&tmp);
    {
        let mut w =
            SessionWriter::open_at(&tmp, cwd, &id, &model, "dsh").map_err(|e| e.to_string())?;
        for e in &events {
            w.append(e);
        }
    }
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(Imported::New)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_keeps_conversation_and_drops_bookkeeping() {
        let rows = vec![
            json!({"type":"session","version":4}),
            json!({"type":"user/message","data":{"content":[{"type":"text","text":"snapshot"}],"source":{"kind":"runtime-context"}}}),
            json!({"type":"user/message","data":{"content":[{"type":"text","text":"hello"}],"source":{"kind":"user"}}}),
            json!({"type":"assistant/message","data":{"message":{"content":[
                {"type":"reasoning","text":"think"},{"type":"text","text":"hi"},
                {"type":"tool-call","id":"c","name":"read","arguments":"{}"}],
                "source":{"model":"m1"}},"usage":{"inputTokens":5}}}),
            json!({"type":"tool/call","data":{"callId":"c","name":"read","arguments":"{}"}}),
            json!({"type":"tool/result","data":{"message":{"toolCallId":"c","content":[{"type":"image"},{"type":"text","text":"out"}]}}}),
            json!({"type":"job/registered","data":{}}),
        ];
        let (ev, _, model) = convert(&rows);
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
        let blocks = ev[1]["data"]["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "tool-call block must be dropped");
        assert_eq!(ev[1]["data"]["usage"]["inputTokens"], 5);
        assert_eq!(
            ev[3]["data"]["message"]["content"][0]["text"],
            "[image]\nout"
        );
        // and the result replays into a valid conversation
        let msgs = store::replay_messages(&ev);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "c");
    }
}
