//! Transport to the harness: one `dsh --profile sdk` child per project, speaking
//! newline-delimited JSON-RPC 2.0 over stdio (see packages/sdk/protocol in the
//! upstream repo). The `Demo` backend emits the same frames from a script so the
//! TUI exercises the identical parse path with no API key.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::thread;

use serde_json::{Value, json};

pub enum Msg {
    /// One raw JSON-RPC line from a project's runtime.
    Frame { project: usize, line: String },
    /// Runtime stderr line (diagnostics).
    Stderr { project: usize, line: String },
    /// Provider balance poll result.
    Balance(Result<crate::balance::Info, String>),
    /// Runtime ended or failed to start.
    Exited { project: usize, why: String },
}

pub struct ProjectSpec {
    pub name: String,
    pub cwd: String,
}

pub trait Backend: Send + Sync {
    /// Write one JSON-RPC frame to the project's runtime.
    fn send(&self, project: usize, frame: Value);
    /// The SDK has no cancel method; the only abort is to drop the process.
    fn kill(&self, project: usize);
    /// Start a runtime for a workspace added after launch.
    fn open(&self, project: usize, spec: &ProjectSpec);
    /// Arm advisor mode for a project: planner and worker model routes.
    /// Backends that cannot re-route ignore this.
    fn configure_advisor(&self, _project: usize, _advisor: &str, _slave: &str) {}
}

#[derive(Default)]
struct Slot {
    stdin: Option<ChildStdin>,
    child: Option<Child>,
}

pub struct Real {
    slots: Mutex<Vec<Slot>>,
    dsh: Vec<String>,
    provider: String,
    model: String,
    tx: Sender<Msg>,
}

impl Real {
    pub fn start(
        specs: &[ProjectSpec],
        dsh: &[String],
        provider: &str,
        model: &str,
        tx: Sender<Msg>,
    ) -> Real {
        let real = Real {
            slots: Mutex::new(vec![]),
            dsh: dsh.to_vec(),
            provider: provider.into(),
            model: model.into(),
            tx,
        };
        for (i, spec) in specs.iter().enumerate() {
            real.open(i, spec);
        }
        real
    }
}

impl Backend for Real {
    fn open(&self, i: usize, spec: &ProjectSpec) {
        let mut argv: Vec<String> = self.dsh.clone();
        if !argv.iter().any(|a| a == "--profile") {
            argv.push("--profile".into());
            argv.push("sdk".into());
        }
        let mut slot = Slot::default();
        let spawned = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        match spawned {
            Ok(mut c) => {
                let out = c.stdout.take().unwrap();
                let err = c.stderr.take().unwrap();
                let mut sin = c.stdin.take().unwrap();
                let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
                    "cwd": spec.cwd, "provider": self.provider, "model": self.model,
                    "approvalRequests": true, "capabilities": {"userQuestions": false}}});
                let _ = writeln!(sin, "{init}");
                let _ = sin.flush();
                let t = self.tx.clone();
                thread::spawn(move || {
                    for line in BufReader::new(out).lines() {
                        match line {
                            Ok(line) => {
                                if t.send(Msg::Frame { project: i, line }).is_err() {
                                    return;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = t.send(Msg::Exited {
                        project: i,
                        why: "runtime closed stdout".into(),
                    });
                });
                let t = self.tx.clone();
                thread::spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        let _ = t.send(Msg::Stderr { project: i, line });
                    }
                });
                slot.stdin = Some(sin);
                slot.child = Some(c);
            }
            Err(e) => {
                let _ = self.tx.send(Msg::Exited {
                    project: i,
                    why: format!("spawn {:?}: {e}", argv[0]),
                });
            }
        }
        let mut slots = self.slots.lock().unwrap();
        while slots.len() <= i {
            slots.push(Slot::default());
        }
        slots[i] = slot;
    }

    fn send(&self, project: usize, frame: Value) {
        if let Some(sin) = self
            .slots
            .lock()
            .unwrap()
            .get_mut(project)
            .and_then(|s| s.stdin.as_mut())
        {
            let _ = writeln!(sin, "{frame}");
            let _ = sin.flush();
        }
    }

    fn kill(&self, project: usize) {
        if let Some(slot) = self.slots.lock().unwrap().get_mut(project) {
            slot.stdin = None;
            if let Some(mut c) = slot.child.take() {
                let _ = c.kill();
            }
        }
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        let n = self.slots.lock().unwrap().len();
        for i in 0..n {
            self.kill(i);
        }
    }
}
