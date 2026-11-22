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

/// Opens the single message that stands in for compacted history.
pub const SUMMARY_PREFIX: &str =
    "[Summary of the earlier conversation, compacted to save context]\n\n";

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
    /// `"destro"`, `"dsh"` (imported from DeepSeek Harness) or `"fork"`.
    pub origin: String,
    /// Planner and worker models when the session ran in advisor mode.
    pub advisor: Option<(String, String)>,
}

/// A saved subagent session belonging to a root session.
#[derive(Clone, Debug)]
pub struct ChildMeta {
    pub id: String,
    /// Session id of the agent that spawned it.
    pub parent: String,
    pub title: String,
    pub path: PathBuf,
}

/// One cross-workspace search result.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub meta: SessionMeta,
    pub snippet: String,
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
        Self::open_with(cwd, id, model, json!({}))
    }

    /// Like [`SessionWriter::open`], merging `extra` (e.g. `{"advisor": {...}}`)
    /// into the header of a new file.
    pub fn open_with(
        cwd: &str,
        id: &str,
        model: &str,
        extra: Value,
    ) -> std::io::Result<SessionWriter> {
        let dir = sessions_dir(cwd).ok_or_else(|| std::io::Error::other("no data directory"))?;
        std::fs::create_dir_all(&dir)?;
        Self::open_at_with(
            &dir.join(format!("{}.jsonl", sanitize_id(id))),
            cwd,
            id,
            model,
            "destro",
            extra,
        )
    }

    /// A subagent's log, filed beside its root session.
    pub fn open_child(
        cwd: &str,
        root_id: &str,
        child_id: &str,
        parent_id: &str,
        model: &str,
    ) -> std::io::Result<SessionWriter> {
        let dir = sessions_dir(cwd).ok_or_else(|| std::io::Error::other("no data directory"))?;
        std::fs::create_dir_all(&dir)?;
        Self::open_at_with(
            &child_path(&dir, root_id, child_id),
            cwd,
            child_id,
            model,
            "destro",
            json!({"parent": parent_id, "root": root_id}),
        )
    }

    pub fn open_at(
        path: &Path,
        cwd: &str,
        id: &str,
        model: &str,
        origin: &str,
    ) -> std::io::Result<SessionWriter> {
        Self::open_at_with(path, cwd, id, model, origin, json!({}))
    }

    pub fn open_at_with(
        path: &Path,
        cwd: &str,
        id: &str,
        model: &str,
        origin: &str,
        extra: Value,
    ) -> std::io::Result<SessionWriter> {
        let fresh = !path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if fresh {
            let mut h = json!({
                "type": HEADER, "id": id, "cwd": cwd, "model": model,
                "created": now_ms(), "origin": origin,
            });
            if let (Some(dst), Some(src)) = (h.as_object_mut(), extra.as_object()) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            writeln!(file, "{h}")?;
        }
        Ok(SessionWriter { file })
    }

    pub fn append(&mut self, event: &Value) -> std::io::Result<()> {
        self.file.write_all(format!("{event}\n").as_bytes())
    }
}

/// Where the root session `id` of workspace `cwd` is (or would be) stored.
pub fn session_path(cwd: &str, id: &str) -> Option<PathBuf> {
    sessions_dir(cwd).map(|d| d.join(format!("{}.jsonl", sanitize_id(id))))
}

/// `<root>.sub.<child>.jsonl`
pub fn child_path(dir: &Path, root_id: &str, child_id: &str) -> PathBuf {
    dir.join(format!(
        "{}.sub.{}.jsonl",
        sanitize_id(root_id),
        sanitize_id(child_id)
    ))
}

fn is_child_file(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(".sub."))
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
    // imported sessions carry their original last-activity time
    let updated_ms = head["updated"].as_u64().unwrap_or(updated_ms);
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
        advisor: head["advisor"]["advisor"]
            .as_str()
            .zip(head["advisor"]["slave"].as_str())
            .map(|(a, b)| (a.to_string(), b.to_string())),
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
        .filter(|p| !is_child_file(p))
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

/// Saved subagent sessions of the root session stored at `root`.
pub fn children(root: &Path) -> Vec<ChildMeta> {
    let (Some(dir), Some(stem)) = (root.parent(), root.file_stem().and_then(|s| s.to_str())) else {
        return vec![];
    };
    let prefix = format!("{stem}.sub.");
    let mut out: Vec<ChildMeta> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".jsonl"))
        })
        .filter_map(|p| {
            let m = read_meta(&p)?;
            let head: Value =
                serde_json::from_str(&BufReader::new(File::open(&p).ok()?).lines().next()?.ok()?)
                    .ok()?;
            Some(ChildMeta {
                id: m.id,
                parent: head["parent"].as_str().unwrap_or("").to_string(),
                title: m.title,
                path: p,
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Give a session a new title (recorded as an event, so it replays).
pub fn rename(path: &Path, title: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().append(true).open(path)?;
    writeln!(
        f,
        "{}",
        json!({"type":"session/title","data":{"title":title}})
    )
}

/// Delete a session and its saved subagent sessions.
pub fn delete(path: &Path) -> std::io::Result<()> {
    for c in children(path) {
        let _ = std::fs::remove_file(c.path);
    }
    std::fs::remove_file(path)
}

/// Copy a session under a new id; the copy is an independent conversation.
pub fn fork(path: &Path, new_id: &str) -> std::io::Result<PathBuf> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let dest = dir.join(format!("{}.jsonl", sanitize_id(new_id)));
    let mut out = File::create(&dest)?;
    let mut lines = BufReader::new(File::open(path)?).lines();
    if let Some(first) = lines.next() {
        let mut head: Value = serde_json::from_str(&first?).unwrap_or_else(|_| json!({}));
        head["type"] = json!(HEADER);
        head["forkedFrom"] = head["id"].clone();
        head["id"] = json!(new_id);
        head["origin"] = json!("fork");
        head["created"] = json!(now_ms());
        head.as_object_mut().map(|o| o.remove("updated"));
        writeln!(out, "{head}")?;
    }
    for l in lines {
        writeln!(out, "{}", l?)?;
    }
    Ok(dest)
}

/// Search prompts and replies of every saved session, all workspaces, newest
/// first. Matching is a case-insensitive substring.
pub fn search(query: &str, limit: usize) -> Vec<SearchHit> {
    let Some(root) = sessions_root() else {
        return vec![];
    };
    search_in(&root, query, limit)
}

fn search_in(root: &Path, query: &str, limit: usize) -> Vec<SearchHit> {
    let q = query.trim();
    if q.is_empty() {
        return vec![];
    }
    let Ok(matcher) = regex::RegexBuilder::new(&regex::escape(q))
        .case_insensitive(true)
        .build()
    else {
        return vec![];
    };
    let mut hits = vec![];
    for ws in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for e in std::fs::read_dir(ws.path()).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") || is_child_file(&p) {
                continue;
            }
            let Ok(f) = File::open(&p) else { continue };
            let mut snippet = None;
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if !(line.contains("\"user/message\"") || line.contains("\"assistant/message\"")) {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let text = match v["type"].as_str() {
                    Some("user/message") => blocks_text(&v["data"]["content"]),
                    _ => blocks_text(&v["data"]["message"]["content"]),
                };
                if let Some(found) = matcher.find(&text) {
                    snippet = Some(excerpt(&text, found.start(), found.end()));
                    break;
                }
            }
            if let (Some(snippet), Some(meta)) = (snippet, read_meta(&p)) {
                hits.push(SearchHit { meta, snippet });
            }
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.meta.updated_ms.max(h.meta.created_ms)));
    hits.truncate(limit);
    hits
}

/// ~100 characters of context around a match, on one line.
fn excerpt(text: &str, at: usize, end_at: usize) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let pos = chars.iter().position(|(i, _)| *i >= at).unwrap_or(0);
    let matched = text[at..end_at].chars().count();
    let start = pos.saturating_sub(40);
    let end = (pos + matched + 60).min(chars.len());
    let mut s: String = chars[start..end].iter().map(|(_, c)| *c).collect();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        s,
        if end < chars.len() { "…" } else { "" }
    )
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
            "compaction/end" => {
                settle(&mut msgs, &mut open);
                let keep = d["keep"].as_u64().unwrap_or(0) as usize;
                let at = msgs.len().saturating_sub(keep);
                let mut tail = msgs.split_off(at);
                // a tail must not open with an orphaned tool result
                while tail.first().is_some_and(|m| m["role"] == "tool") {
                    tail.remove(0);
                }
                msgs = vec![json!({
                    "role": "user",
                    "content": format!("{SUMMARY_PREFIX}{}", d["summary"].as_str().unwrap_or("")),
                })];
                msgs.extend(tail);
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
    fn rename_fork_children_and_delete() {
        let dir = std::env::temp_dir().join(format!("destro-store2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("r1.jsonl");
        let mut w = SessionWriter::open_at_with(
            &root,
            "/w",
            "r1",
            "m",
            "destro",
            json!({"advisor": {"advisor": "big", "slave": "small"}}),
        )
        .unwrap();
        w.append(&json!({"type":"user/message","data":{"content":[{"type":"text","text":"find the needle in haystack"}],"source":{"kind":"user"}}})).unwrap();
        let cp = child_path(&dir, "r1", "session-000002");
        let mut cw = SessionWriter::open_at_with(
            &cp,
            "/w",
            "session-000002",
            "m",
            "destro",
            json!({"parent":"r1","root":"r1"}),
        )
        .unwrap();
        cw.append(&json!({"type":"session/title","data":{"title":"worker"}}))
            .unwrap();

        let meta = read_meta(&root).unwrap();
        assert_eq!(meta.advisor, Some(("big".into(), "small".into())));
        let kids = children(&root);
        assert_eq!(kids.len(), 1);
        assert_eq!(
            (
                kids[0].id.as_str(),
                kids[0].parent.as_str(),
                kids[0].title.as_str()
            ),
            ("session-000002", "r1", "worker")
        );
        assert!(is_child_file(&cp));

        rename(&root, "better title").unwrap();
        assert_eq!(read_meta(&root).unwrap().title, "better title");

        let forked = fork(&root, "r2").unwrap();
        let fm = read_meta(&forked).unwrap();
        assert_eq!(
            (fm.id.as_str(), fm.origin.as_str(), fm.title.as_str()),
            ("r2", "fork", "better title")
        );
        assert_eq!(load(&forked).len(), load(&root).len());

        delete(&root).unwrap();
        assert!(!root.exists() && !cp.exists());
        assert!(forked.exists(), "the fork is independent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn excerpt_centres_on_the_match() {
        let t = "alpha beta gamma delta needle epsilon zeta";
        let at = t.find("needle").unwrap();
        let e = excerpt(t, at, at + 6);
        assert!(e.contains("needle"));
        let long = format!("{} needle {}", "x".repeat(200), "y".repeat(200));
        let at = long.find("needle").unwrap();
        let e = excerpt(&long, at, at + 6);
        assert!(e.starts_with('…') && e.ends_with('…') && e.contains("needle"));
        assert!(e.chars().count() < 130);
    }

    #[test]
    fn history_search_handles_length_changing_unicode() {
        let root = std::env::temp_dir().join(format!("destro-search-{}", std::process::id()));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("chat.jsonl");
        let mut writer = SessionWriter::open_at(&path, "/w", "chat", "m", "destro").unwrap();
        writer
            .append(&json!({"type":"user/message","data":{"content":"İé東京"}}))
            .unwrap();
        drop(writer);
        let hits = search_in(&root, "É東", 10);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "İé東京");
    }

    #[test]
    fn replay_applies_compaction() {
        let ev = |t: &str, d: Value| json!({"type": t, "data": d});
        let mut events = vec![];
        for i in 0..4 {
            events.push(ev("user/message", json!({"content":[{"type":"text","text":format!("q{i}")}],"source":{"kind":"user"}})));
            events.push(ev(
                "assistant/message",
                json!({"message":{"content":[{"type":"text","text":format!("a{i}")}]}}),
            ));
        }
        events.push(ev("compaction/end", json!({"summary":"S","keep":2})));
        events.push(ev(
            "user/message",
            json!({"content":[{"type":"text","text":"after"}],"source":{"kind":"user"}}),
        ));
        let m = replay_messages(&events);
        let texts: Vec<&str> = m.iter().map(|x| x["content"].as_str().unwrap()).collect();
        assert!(texts[0].starts_with(SUMMARY_PREFIX) && texts[0].ends_with("S"));
        assert_eq!(&texts[1..], ["q3", "a3", "after"]);
    }

    #[test]
    fn writer_roundtrip() {
        let dir = std::env::temp_dir().join(format!("destro-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let mut w = SessionWriter::open_at(&p, "/w", "s1", "m", "destro").unwrap();
        w.append(&json!({"type":"session/title","data":{"title":"hello"}}))
            .unwrap();
        w.append(&json!({"type":"turn/start","data":{"turn":1}}))
            .unwrap();
        let meta = read_meta(&p).unwrap();
        assert_eq!(
            (meta.title.as_str(), meta.turns, meta.id.as_str()),
            ("hello", 1, "s1")
        );
        assert_eq!(load(&p).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
