//! Scripted stand-in for `dsh --profile sdk`. It emits frames shaped like the
//! upstream SDK protocol (`session.event`, `session.status`, `subagent.*`,
//! `approval/request`) so the TUI runs the real parse path without an API key.
//! Tool names and prose here are illustrative, NOT recorded from a real model.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::backend::{Backend, Msg};

pub struct Step {
    pub t: u64,
    /// Held until the approval with this rpc id has been answered.
    pub gate: Option<u64>,
    pub frame: String,
}

#[derive(Default)]
pub struct Gen {
    seq: HashMap<String, u64>,
    n: u64,
}

impl Gen {
    fn next_id(&mut self, p: &str) -> String {
        self.n += 1;
        format!("{p}-{}", self.n)
    }
    fn notif(method: &str, params: Value) -> String {
        json!({"jsonrpc":"2.0","method":method,"params":params}).to_string()
    }
    fn ev(&mut self, sid: &str, ty: &str, data: Value) -> String {
        let s = self.seq.entry(sid.to_string()).or_insert(0);
        *s += 1;
        let seq = *s;
        Self::notif(
            "session.event",
            json!({"sessionId":sid,"event":{"type":ty,"seq":seq,"time":1_790_000_000_000u64 + seq*1000,"data":data}}),
        )
    }
    pub fn status(&self, sid: &str, st: &str) -> String {
        Self::notif("session.status", json!({"sessionId":sid,"status":st}))
    }
    fn turn(&mut self, sid: &str, turn: u32, start: bool) -> String {
        if start {
            self.ev(sid, "turn/start", json!({"turn":turn}))
        } else {
            self.ev(
                sid,
                "turn/end",
                json!({"turn":turn,"reason":{"kind":"stop"}}),
            )
        }
    }
    pub fn user(&mut self, sid: &str, _turn: u32, text: &str, kind: &str) -> String {
        let id = self.next_id("m");
        self.ev(sid, "user/message", json!({"id":id,"role":"user","content":[{"type":"text","text":text}],"source":{"kind":kind}}))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn assistant(
        &mut self,
        sid: &str,
        turn: u32,
        reasoning: Option<&str>,
        text: Option<&str>,
        usage: (u64, u64, u64),
    ) -> String {
        let id = self.next_id("m");
        let mut content = vec![];
        if let Some(r) = reasoning {
            content.push(json!({"type":"reasoning","text":r}));
        }
        if let Some(t) = text {
            content.push(json!({"type":"text","text":t}));
        }
        self.ev(sid, "assistant/message", json!({"turn":turn,"step":1,"message":{"id":id,"role":"assistant","content":content,"source":{"kind":"model"}},
            "stream":[],"usage":{"inputTokens":usage.0,"outputTokens":usage.1,"cacheReadTokens":usage.2}}))
    }
    pub fn call(&mut self, sid: &str, turn: u32, name: &str, args: Value) -> (String, String) {
        let id = self.next_id("call");
        let f = self.ev(
            sid,
            "tool/call",
            json!({"turn":turn,"step":1,"callId":id,"name":name,"arguments":args.to_string()}),
        );
        (id, f)
    }
    pub fn result(
        &mut self,
        sid: &str,
        turn: u32,
        call_id: &str,
        text: &str,
        is_error: bool,
    ) -> String {
        let id = self.next_id("m");
        self.ev(sid, "tool/result", json!({"turn":turn,"step":1,"message":{"id":id,"role":"tool","content":[{"type":"text","text":text}],
            "source":{"kind":"tool"},"toolCallId":call_id,"isError":is_error}}))
    }
    pub fn ctx(&mut self, sid: &str, window: u64) -> String {
        self.ev(sid, "request/context", json!({"provider":"deepseek-official","model":"deepseek-v4-pro","contextWindow":window}))
    }
    pub fn title(&mut self, sid: &str, t: &str) -> String {
        self.ev(sid, "session/title", json!({"title":t}))
    }
    fn sub_started(parent: &str, child: &str) -> String {
        Self::notif(
            "subagent.started",
            json!({"parentSessionId":parent,"childSessionId":child}),
        )
    }
    fn sub_finished(parent: &str, child: &str, ok: bool) -> String {
        Self::notif(
            "subagent.finished",
            json!({"provider":"local","agentId":child,"parentSessionId":parent,"childSessionId":child,
            "status": if ok {"ok"} else {"error"},"stopReason":{"kind":"completed"}}),
        )
    }
    fn approval(id: u64, sid: &str, tool: &str, reason: &str) -> String {
        json!({"jsonrpc":"2.0","id":id,"method":"approval/request","params":{"sessionId":sid,"toolName":tool,"callId":"call-x","reason":reason}}).to_string()
    }
}

fn s(t: u64, frame: String) -> Step {
    Step {
        t,
        gate: None,
        frame,
    }
}
fn g(t: u64, gate: u64, frame: String) -> Step {
    Step {
        t,
        gate: Some(gate),
        frame,
    }
}

/// Parent turn that fans out to three parallel subagents, one of which blocks on an approval.
pub fn fanout_chains(scr: &mut Gen, sid: &str, prompt: &str, approval_id: u64) -> Vec<Vec<Step>> {
    let (c1, c2, c3) = (
        format!("{sid}.map"),
        format!("{sid}.tests"),
        format!("{sid}.store"),
    );
    let mut parent = vec![];
    parent.push(s(0, scr.status(sid, "running")));
    parent.push(s(0, scr.turn(sid, 1, true)));
    parent.push(s(0, scr.user(sid, 1, prompt, "user")));
    parent.push(s(350, scr.assistant(sid, 1,
        Some("Three independent questions: where the middleware reads credentials, whether the suite is green before I touch it, and how sessions are persisted. Run them in parallel and merge."),
        Some("Splitting this into three parallel investigations before changing anything: a call-site map, a test baseline, and the session-store schema."),
        (18_240, 212, 0))));
    let (id, f) = scr.call(sid, 1, "todo_write", json!({"todos":["map auth call sites","baseline tests","inspect session store","write migration"]}));
    parent.push(s(700, f));
    parent.push(s(
        760,
        scr.result(sid, 1, &id, "todo list updated (4 items)", false),
    ));
    for (i, c) in [&c1, &c2, &c3].iter().enumerate() {
        parent.push(s(1000 + i as u64 * 120, Gen::sub_started(sid, c)));
    }

    let mut a = vec![];
    a.push(s(1100, scr.status(&c1, "running")));
    a.push(s(1100, scr.turn(&c1, 1, true)));
    a.push(s(
        1100,
        scr.user(
            &c1,
            1,
            "Map every call site that reads or writes auth credentials.",
            "subagent",
        ),
    ));
    let (id, f) = scr.call(
        &c1,
        1,
        "grep",
        json!({"pattern":"authenticate\\(|req\\.user|getSession","path":"src/"}),
    );
    a.push(s(1700, f));
    a.push(s(2100, scr.result(&c1, 1, &id, "src/middleware/auth.ts:41  const user = await authenticate(req)\nsrc/routes/watch.ts:18   if (!req.user) return 401\nsrc/routes/admin.ts:9    requireRole(req.user, 'admin')\n... 14 more matches in 9 files", false)));
    let (id, f) = scr.call(
        &c1,
        1,
        "read_file",
        json!({"path":"src/middleware/auth.ts"}),
    );
    a.push(s(2500, f));
    a.push(s(2700, scr.result(&c1, 1, &id, "export async function authenticate(req) {\n  const token = req.headers.authorization?.slice(7)\n  return verifyJwt(token) // stateless; no revocation\n}", false)));
    a.push(s(4300, scr.assistant(&c1, 1, None, Some("17 call sites in 11 files. All go through `authenticate()` except `routes/ws.ts`, which decodes the JWT itself — that one needs separate handling."), (24_900, 164, 18_000))));
    a.push(s(4400, scr.turn(&c1, 1, false)));
    a.push(s(4400, scr.status(&c1, "idle")));
    a.push(s(4450, Gen::sub_finished(sid, &c1, true)));

    let mut b = vec![];
    b.push(s(1220, scr.status(&c2, "running")));
    b.push(s(1220, scr.turn(&c2, 1, true)));
    b.push(s(
        1220,
        scr.user(
            &c2,
            1,
            "Run the test suite and report the baseline.",
            "subagent",
        ),
    ));
    let (id, f) = scr.call(&c2, 1, "bash", json!({"command":"cargo test --workspace"}));
    b.push(s(1800, f));
    b.push(s(
        1850,
        Gen::approval(
            approval_id,
            &c2,
            "bash",
            "run `cargo test --workspace` outside the sandbox write boundary",
        ),
    ));
    b.push(g(2000, approval_id, scr.result(&c2, 1, &id, "running 212 tests\ntest result: ok. 209 passed; 3 failed; 0 ignored\nfailed: auth::refresh_rotates, auth::expired_cookie, store::concurrent_write", false)));
    b.push(g(2400, approval_id, scr.assistant(&c2, 1, None, Some("Baseline: 209/212 passing. 3 pre-existing failures, all in auth/store — they are the tests the migration should fix."), (21_100, 118, 14_000))));
    b.push(g(2500, approval_id, scr.turn(&c2, 1, false)));
    b.push(g(2500, approval_id, scr.status(&c2, "idle")));
    b.push(g(2550, approval_id, Gen::sub_finished(sid, &c2, true)));

    let mut c = vec![];
    c.push(s(1340, scr.status(&c3, "running")));
    c.push(s(1340, scr.turn(&c3, 1, true)));
    c.push(s(
        1340,
        scr.user(
            &c3,
            1,
            "Inspect the session store schema and propose a migration.",
            "subagent",
        ),
    ));
    let (id, f) = scr.call(&c3, 1, "read_file", json!({"path":"db/schema.sql"}));
    c.push(s(1900, f));
    c.push(s(2050, scr.result(&c3, 1, &id, "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE, pw_hash TEXT);\n-- no sessions table", false)));
    let (id, f) = scr.call(
        &c3,
        1,
        "edit_file",
        json!({"path":"db/migrations/0007_sessions.sql","create":true}),
    );
    c.push(s(3300, f));
    c.push(s(3500, scr.result(&c3, 1, &id, "--- a/db/migrations/0007_sessions.sql\n+++ b/db/migrations/0007_sessions.sql\n@@ -0,0 +1,8 @@\n+CREATE TABLE sessions (\n+  id         TEXT PRIMARY KEY,\n+  user_id    INTEGER NOT NULL REFERENCES users(id),\n+  expires_at INTEGER NOT NULL,\n+  revoked_at INTEGER\n+);\n+CREATE INDEX sessions_user ON sessions(user_id);", false)));
    c.push(s(5200, scr.assistant(&c3, 1, None, Some("Added `sessions(id, user_id, expires_at, revoked_at)` with an index on `user_id`. Migration is forward-only and idempotent."), (27_300, 201, 21_000))));
    c.push(s(5300, scr.turn(&c3, 1, false)));
    c.push(s(5300, scr.status(&c3, "idle")));
    c.push(s(5350, Gen::sub_finished(sid, &c3, true)));

    parent.push(g(6000, approval_id, scr.assistant(sid, 1,
        Some("All three are in. The three failing tests are the ones the new store fixes, so the plan is: migration, then swap authenticate() to read the session row, then handle ws.ts."),
        Some("Findings merged.\n\n• 17 auth call sites in 11 files; `routes/ws.ts` bypasses `authenticate()`.\n• Baseline 209/212; the 3 failures are exactly the refresh/expiry/concurrency cases.\n• Migration `0007_sessions.sql` is drafted.\n\nNext: switch `authenticate()` to session-token lookup, then port `ws.ts`.\n\n**Proposed** `authenticate()`:\n```ts\nexport async function authenticate(req: Request) {\n  const id = readSessionCookie(req) // opaque token, not a JWT\n  const row = await db.get<Session>(\"SELECT * FROM sessions WHERE id = ?\", id)\n  if (!row || row.revoked_at || row.expires_at < Date.now()) return null\n  return loadUser(row.user_id) // revocation takes effect immediately\n}\n```\nRun the baseline again after the swap:\n```bash\ncargo test --workspace -- auth:: store::\n```\nSay the word and I'll start."),
        (31_800, 389, 24_000))));
    parent.push(g(6100, approval_id, scr.turn(sid, 1, false)));
    parent.push(g(6100, approval_id, scr.status(sid, "idle")));
    vec![parent, a, b, c]
}

/// A long, finished conversation: many turns of small tool use. Instant (t = 0).
pub fn history(
    scr: &mut Gen,
    sid: &str,
    title: &str,
    turns: u32,
    window: u64,
    ctx_used: u64,
) -> Vec<Step> {
    let mut v = vec![s(0, scr.title(sid, title)), s(0, scr.ctx(sid, window))];
    for t in 1..=turns {
        v.push(s(0, scr.turn(sid, t, true)));
        v.push(s(0, scr.user(sid, t, &format!("step {t}: continue with the next file in the import queue and keep the diff small"), "user")));
        let (id, f) = scr.call(
            sid,
            t,
            "read_file",
            json!({"path": format!("src/importers/rates_{t:03}.rs")}),
        );
        v.push(s(0, f));
        v.push(s(
            0,
            scr.result(
                sid,
                t,
                &id,
                &format!("// rates_{t:03}.rs\npub fn parse(row: &Row) -> Rate {{ /* ... */ }}"),
                false,
            ),
        ));
        v.push(s(0, scr.assistant(sid, t, None, Some(&format!("Done with queue item {t}. Normalised the decimal parsing and added a regression test.")),
            (ctx_used * t as u64 / turns as u64, 90, 0))));
        v.push(s(0, scr.turn(sid, t, false)));
    }
    v
}

// ----- live backend -------------------------------------------------------------------------

struct Broker {
    replied: Mutex<HashSet<u64>>,
    cv: Condvar,
}

pub struct Demo {
    tx: Sender<Msg>,
    broker: Arc<Broker>,
    scr: Arc<Mutex<Gen>>,
    next_approval: Arc<Mutex<u64>>,
}

impl Demo {
    pub fn start(tx: Sender<Msg>, projects: usize) -> Demo {
        let d = Demo {
            tx,
            broker: Arc::new(Broker {
                replied: Mutex::new(HashSet::new()),
                cv: Condvar::new(),
            }),
            scr: Arc::new(Mutex::new(Gen::default())),
            next_approval: Arc::new(Mutex::new(100)),
        };
        for p in 0..projects {
            d.boot(p);
        }
        d
    }

    fn boot(&self, p: usize) {
        let _ = self.tx.send(Msg::Frame { project: p, line: json!({"jsonrpc":"2.0","id":0,"result":{"serverInfo":{"name":"deepseek-harness-sdk-runtime","version":"demo"}}}).to_string() });
        let steps = boot_steps(
            &mut self.scr.lock().unwrap(),
            p,
            &mut self.next_approval.lock().unwrap(),
        );
        for chain in steps {
            self.run_chain(p, chain);
        }
    }

    fn run_chain(&self, project: usize, chain: Vec<Step>) {
        let tx = self.tx.clone();
        let broker = self.broker.clone();
        thread::spawn(move || {
            let start = Instant::now();
            for step in chain {
                let due = Duration::from_millis(step.t);
                if let Some(rest) = due.checked_sub(start.elapsed()) {
                    thread::sleep(rest);
                }
                if let Some(id) = step.gate {
                    let mut done = broker.replied.lock().unwrap();
                    while !done.contains(&id) {
                        done = broker.cv.wait(done).unwrap();
                    }
                }
                if tx
                    .send(Msg::Frame {
                        project,
                        line: step.frame,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }
}

impl Backend for Demo {
    fn send(&self, project: usize, frame: Value) {
        if frame.get("result").is_some() && frame.get("method").is_none() {
            if let Some(id) = frame["id"].as_u64() {
                self.broker.replied.lock().unwrap().insert(id);
                self.broker.cv.notify_all();
            }
            return;
        }
        if frame["method"] == "session/query" {
            let sid = frame["params"]["sessionId"]
                .as_str()
                .unwrap_or("s")
                .to_string();
            let text = frame["params"]["contentBlocks"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let aid = {
                let mut n = self.next_approval.lock().unwrap();
                *n += 1;
                *n
            };
            let chains = fanout_chains(&mut self.scr.lock().unwrap(), &sid, &text, aid);
            let _ = self.tx.send(Msg::Frame { project, line: json!({"jsonrpc":"2.0","id":frame["id"],"result":{"kind":"prompt","messageId":"m-q"}}).to_string() });
            for c in chains {
                self.run_chain(project, c);
            }
        }
    }
    fn kill(&self, _project: usize) {}
    fn open(&self, project: usize, _spec: &crate::backend::ProjectSpec) {
        let _ = self.tx.send(Msg::Frame { project, line: json!({"jsonrpc":"2.0","id":0,"result":{"serverInfo":{"name":"deepseek-harness-sdk-runtime","version":"demo"}}}).to_string() });
    }
}

/// Per-project startup content. Returns chains (each its own thread live; flattened for snapshots).
pub fn boot_steps(scr: &mut Gen, p: usize, next_approval: &mut u64) -> Vec<Vec<Step>> {
    match p {
        0 => {
            *next_approval += 1;
            let sid = "kuro-auth-1";
            let mut head = vec![
                s(0, scr.title(sid, "auth middleware → session tokens")),
                s(0, scr.ctx(sid, 1_000_000)),
            ];
            let mut chains = fanout_chains(
                scr,
                sid,
                "refactor the auth middleware to use revocable session tokens. split the investigation across agents before touching code.",
                *next_approval,
            );
            head.append(&mut chains[0]);
            chains[0] = head;
            for c in chains.iter_mut() {
                for st in c.iter_mut() {
                    st.t += 400;
                }
            }
            chains
        }
        1 => {
            let mut v = history(
                scr,
                "naf-import-1",
                "NAF rate-sheet importer",
                6,
                1_000_000,
                96_000,
            );
            v.push(s(0, scr.status("naf-import-1", "idle")));
            let mut w = vec![
                s(0, scr.title("naf-notes-2", "reverse-mortgage copy review")),
                s(0, scr.ctx("naf-notes-2", 1_000_000)),
            ];
            w.push(s(0, scr.status("naf-notes-2", "idle")));
            vec![v, w]
        }
        _ => {
            let sid = "roar-fw-1";
            let mut v = history(
                scr,
                sid,
                "ROAR-MESH firmware: LoRa duty-cycle",
                120,
                128_000,
                118_400,
            );
            v.push(s(0, scr.status(sid, "running")));
            v.push(s(0, scr.turn(sid, 121, true)));
            v.push(s(
                0,
                scr.user(
                    sid,
                    121,
                    "build release for the esp32-s3 target and flash the bench unit",
                    "user",
                ),
            ));
            let (_, f) = scr.call(
                sid,
                121,
                "bash",
                json!({"command":"cargo build --release --target xtensa-esp32s3-none-elf"}),
            );
            v.push(s(0, f));
            vec![v]
        }
    }
}
