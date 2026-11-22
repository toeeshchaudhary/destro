//! Durable sessions: one append-only JSONL file per root session.
//!
//! Layout: `<data dir>/sessions/<workspace slug>/<session id>.jsonl`. The first
//! line is a header (`{"type":"destro/session", id, cwd, model, created}`); every
//! later line is a session event exactly as it crossed the wire
//! (`{"type","data"}`), so the TUI replays a file through the same code that
//! renders live events, and [`replay_messages`] rebuilds the model-facing
//! conversation so a resumed agent continues where it stopped.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::context::data_dir;

pub const HEADER: &str = "destro/session";

/// Summary of one stored session, cheap enough to list in a picker.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub model: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub turns: u32,
    pub path: PathBuf,
    /// `"destro"`, or `"dsh"` for sessions imported from DeepSeek Harness.
    pub origin: String,
}

/// Filesystem-safe, human-recognisable, collision-resistant folder name.
pub fn workspace_slug(cwd: &str) -> String {
    let norm = cwd.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let mut slug: String = norm
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let tail: String = slug
        .chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in norm.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{tail}-{:08x}", h as u32)
}

pub fn sessions_root() -> Option<PathBuf> {
    data_dir().map(|d| d.join("sessions"))
}

pub fn sessions_dir(cwd: &str) -> Option<PathBuf> {
    sessions_root().map(|r| r.join(workspace_slug(cwd)))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Appends events to one session file, flushing every line so a crash loses
/// nothing already shown.
pub struct SessionWriter {
    file: File,
}

impl SessionWriter {
    /// Start (or continue) the file for `id`. The header is written once.
    pub fn open(cwd: &str, id: &str, model: &str) -> std::io::Result<SessionWriter> {
        let dir = sessions_dir(cwd).ok_or_else(|| std::io::Error::other("no data directory"))?;
        std::fs::create_dir_all(&dir)?;
        Self::open_at(
            &dir.join(format!("{}.jsonl", sanitize_id(id))),
            cwd,
            id,
            model,
            "destro",
        )
    }

    pub fn open_at(
        path: &Path,
        cwd: &str,
        id: &str,
        model: &str,
        origin: &str,
    ) -> std::io::Result<SessionWriter> {
        let fresh = !path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if fresh {
            let h = json!({
                "type": HEADER, "id": id, "cwd": cwd, "model": model,
                "created": now_ms(), "origin": origin,
            });
            writeln!(file, "{h}")?;
        }
        Ok(SessionWriter { file })
    }

    pub fn append(&mut self, event: &Value) {
        let _ = writeln!(self.file, "{event}");
    }
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Header + cheap scan for title and turn count.
pub fn read_meta(path: &Path) -> Option<SessionMeta> {
    let f = File::open(path).ok()?;
    let updated_ms = f
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut lines = BufReader::new(f).lines();
    let head: Value = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    if head["type"] != HEADER {
        return None;
    }
    let mut title = String::new();
    let mut first_user = String::new();
    let mut turns = 0;
    for line in lines.map_while(Result::ok) {
        if line.contains("\"turn/start\"") {
            turns += 1;
        } else if line.contains("\"session/title\"") {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && let Some(t) = v["data"]["title"].as_str()
            {
                title = t.to_string();
            }
        } else if first_user.is_empty()
            && line.contains("\"user/message\"")
            && let Ok(v) = serde_json::from_str::<Value>(&line)
        {
            first_user = blocks_text(&v["data"]["content"]);
        }
    }
    if title.is_empty() {
        title = first_user
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
    }
    if title.is_empty() {
        title = "(empty session)".into();
    }
    Some(SessionMeta {
        id: head["id"].as_str().unwrap_or("").to_string(),
        cwd: head["cwd"].as_str().unwrap_or("").to_string(),
        title,
        model: head["model"].as_str().unwrap_or("").to_string(),
        created_ms: head["created"].as_u64().unwrap_or(0),
        updated_ms,
        turns,
        path: path.to_path_buf(),
        origin: head["origin"].as_str().unwrap_or("destro").to_string(),
    })
}

/// Stored sessions for a workspace, newest first.
pub fn list(cwd: &str) -> Vec<SessionMeta> {
    let Some(dir) = sessions_dir(cwd) else {
        return vec![];
    };
    let mut out: Vec<SessionMeta> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|p| read_meta(&p))
        .collect();
    out.sort_by_key(|m| std::cmp::Reverse(m.updated_ms.max(m.created_ms)));
    out
}

/// Every event in a session file (header and unreadable lines skipped).
pub fn load(path: &Path) -> Vec<Value> {
    let Ok(f) = File::open(path) else {
        return vec![];
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v["type"] != HEADER)
        .collect()
}

pub fn blocks_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_string),
                Some("image") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Rebuild the model-facing conversation from stored events.
///
/// Only human prompts, assistant text, tool calls and tool results survive;
/// reasoning is dropped (providers do not want it echoed back). A tool call with
/// no recorded result (the session was interrupted) gets a stub result so the
/// next request is well-formed.
pub fn replay_messages(events: &[Value]) -> Vec<Value> {
    let mut msgs: Vec<Value> = vec![];
    let mut open: Vec<(String, String)> = vec![]; // call id, name
    let settle = |msgs: &mut Vec<Value>, open: &mut Vec<(String, String)>| {
        for (id, name) in open.drain(..) {
            msgs.push(json!({"role":"tool","tool_call_id":id,"name":name,"content":"[no result recorded: interrupted]"}));
        }
    };
    for ev in events {
        let d = &ev["data"];
        match ev["type"].as_str().unwrap_or("") {
            "user/message" => {
                let kind = d["source"]["kind"].as_str().unwrap_or("user");
                if kind == "user" || kind.is_empty() {
                    settle(&mut msgs, &mut open);
                    msgs.push(json!({"role":"user","content":blocks_text(&d["content"])}));
                }
            }
            "assistant/message" => {
                settle(&mut msgs, &mut open);
                let text: String = d["message"]["content"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                msgs.push(json!({"role":"assistant","content":text}));
            }
            "tool/call" => {
                let id = d["callId"].as_str().unwrap_or("").to_string();
                let name = d["name"].as_str().unwrap_or("").to_string();
                let call = json!({"id":id,"type":"function","function":{"name":name,"arguments":d["arguments"].as_str().unwrap_or("{}")}});
                match msgs.last_mut() {
                    Some(m) if m["role"] == "assistant" => {
                        if m.get("tool_calls").is_none() {
                            m["tool_calls"] = json!([]);
                        }
                        m["tool_calls"].as_array_mut().unwrap().push(call);
                    }
                    _ => msgs.push(json!({"role":"assistant","content":"","tool_calls":[call]})),
                }
                open.push((id, name));
            }
            "tool/result" => {
                let m = &d["message"];
                let id = m["toolCallId"].as_str().unwrap_or("").to_string();
                if let Some(i) = open.iter().position(|(c, _)| *c == id) {
                    let (_, name) = open.remove(i);
                    msgs.push(json!({"role":"tool","tool_call_id":id,"name":name,"content":blocks_text(&m["content"])}));
                }
            }
            _ => {}
        }
    }
    settle(&mut msgs, &mut open);
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_stable_and_distinguishes_paths() {
        let a = workspace_slug(r"C:\Users\x\proj");
        assert_eq!(a, workspace_slug("c:/users/x/proj/"));
        assert_ne!(a, workspace_slug(r"C:\Users\x\proj2"));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn replay_rebuilds_calls_and_stubs_missing_results() {
        let ev = |t: &str, d: Value| json!({"type": t, "data": d});
        let events = vec![
            ev(
                "user/message",
                json!({"content":[{"type":"text","text":"hi"}],"source":{"kind":"user"}}),
            ),
            ev(
                "assistant/message",
                json!({"message":{"content":[{"type":"reasoning","text":"hm"},{"type":"text","text":"ok"}]}}),
            ),
            ev(
                "tool/call",
                json!({"callId":"c1","name":"read","arguments":"{}"}),
            ),
            ev(
                "tool/result",
                json!({"message":{"toolCallId":"c1","content":[{"type":"text","text":"data"}]}}),
            ),
            ev(
                "assistant/message",
                json!({"message":{"content":[{"type":"text","text":"done"}]}}),
            ),
            ev(
                "tool/call",
                json!({"callId":"c2","name":"pwsh","arguments":"{}"}),
            ),
        ];
        let m = replay_messages(&events);
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant", "tool"]);
        assert_eq!(m[1]["content"], "ok");
        assert_eq!(m[1]["tool_calls"][0]["id"], "c1");
        assert_eq!(m[2]["content"], "data");
        assert!(m[4]["content"].as_str().unwrap().contains("interrupted"));
    }

    #[test]
    fn writer_roundtrip() {
        let dir = std::env::temp_dir().join(format!("destro-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let mut w = SessionWriter::open_at(&p, "/w", "s1", "m", "destro").unwrap();
        w.append(&json!({"type":"session/title","data":{"title":"hello"}}));
        w.append(&json!({"type":"turn/start","data":{"turn":1}}));
        let meta = read_meta(&p).unwrap();
        assert_eq!(
            (meta.title.as_str(), meta.turns, meta.id.as_str()),
            ("hello", 1, "s1")
        );
        assert_eq!(load(&p).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
