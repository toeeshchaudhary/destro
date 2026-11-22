//! Application state, protocol dispatch and key handling.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::backend::{Backend, Msg, ProjectSpec};
use crate::balance::Balance;
use crate::model::{Approval, Conn, Item, Project, Status};
use crate::panel::{self, Kind, Panel, Tone};
use crate::theme;
use crate::ui::wrap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Project(usize),
    /// project, session, depth
    Session(usize, usize, usize),
}

pub struct SnipState {
    pub sel: usize,
    pub v: usize,
    pub h: usize,
}

struct TabState {
    cands: Vec<String>,
    idx: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Advisor,
}

/// A slash command, shown in the `/` popup and the Ctrl+P palette.
pub struct Cmd {
    pub name: &'static str,
    pub keys: &'static str,
    pub desc: &'static str,
    /// Takes an argument: choosing it prefills the prompt instead of running.
    pub arg: bool,
}

pub const COMMANDS: &[Cmd] = &[
    Cmd {
        name: "/new",
        keys: "Alt+N",
        desc: "start a new session in this workspace",
        arg: false,
    },
    Cmd {
        name: "/resume",
        keys: "Alt+R",
        desc: "continue a saved session",
        arg: false,
    },
    Cmd {
        name: "/model",
        keys: "Alt+M",
        desc: "switch model or provider (or /model <route>)",
        arg: false,
    },
    Cmd {
        name: "/open",
        keys: "Alt+O",
        desc: "add a workspace folder",
        arg: true,
    },
    Cmd {
        name: "/close",
        keys: "",
        desc: "close this workspace and stop its runtime",
        arg: false,
    },
    Cmd {
        name: "/theme",
        keys: "Ctrl+T",
        desc: "pick a colour theme (or /theme <name>)",
        arg: false,
    },
    Cmd {
        name: "/skills",
        keys: "",
        desc: "browse skills and custom commands",
        arg: false,
    },
    Cmd {
        name: "/mcp",
        keys: "",
        desc: "MCP servers: approve, enable, reconnect",
        arg: false,
    },
    Cmd {
        name: "/context",
        keys: "",
        desc: "show the AGENTS.md / CLAUDE.md files in use",
        arg: false,
    },
    Cmd {
        name: "/import",
        keys: "",
        desc: "bring in chats from DeepSeek Harness (dsh)",
        arg: false,
    },
    Cmd {
        name: "/export",
        keys: "",
        desc: "save this session as Markdown",
        arg: false,
    },
    Cmd {
        name: "/advisor",
        keys: "Alt+A",
        desc: "advisor mode: planner + worker models",
        arg: false,
    },
    Cmd {
        name: "/normal",
        keys: "",
        desc: "leave advisor mode",
        arg: false,
    },
    Cmd {
        name: "/snippets",
        keys: "Ctrl+S",
        desc: "browse code from this session",
        arg: false,
    },
    Cmd {
        name: "/copy",
        keys: "",
        desc: "copy the last reply",
        arg: false,
    },
    Cmd {
        name: "/sidebar",
        keys: "Ctrl+B",
        desc: "toggle the workspace sidebar",
        arg: false,
    },
    Cmd {
        name: "/panel",
        keys: "Ctrl+G",
        desc: "toggle the agents panel",
        arg: false,
    },
    Cmd {
        name: "/kill",
        keys: "Ctrl+K",
        desc: "abort this workspace's runtime",
        arg: false,
    },
    Cmd {
        name: "/help",
        keys: "F1",
        desc: "show every key binding",
        arg: false,
    },
    Cmd {
        name: "/quit",
        keys: "Ctrl+Q",
        desc: "exit destro",
        arg: false,
    },
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Builtin,
    Skill,
    Command,
}

/// One row of the `/` popup or palette: a built-in, a skill, or a custom command.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub keys: String,
    pub desc: String,
    pub arg: bool,
    pub kind: EntryKind,
}

/// Ctrl+P command palette state.
pub struct Palette {
    pub query: String,
    pub sel: usize,
}

/// Ctrl+T theme picker; `original` is restored on Esc.
pub struct ThemePick {
    pub sel: usize,
    pub original: usize,
}

/// Models offered before the user types their own.
pub const MODEL_SUGGESTIONS: [&str; 4] = [
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    "deepseek-chat",
    "deepseek-reasoner",
];

/// The start-of-advisor picker: which model plans, which models execute.
#[derive(Clone, Debug)]
pub struct AdvisorSetup {
    pub models: [String; 2],
    pub focus: usize,
}

impl AdvisorSetup {
    /// Prefill from the selected provider's models, falling back to defaults.
    pub fn from_suggestions(suggestions: &[String]) -> Self {
        let pick = |i: usize, fallback: &str| {
            suggestions
                .get(i)
                .cloned()
                .unwrap_or_else(|| fallback.to_string())
        };
        AdvisorSetup {
            models: [pick(0, MODEL_SUGGESTIONS[0]), pick(1, MODEL_SUGGESTIONS[1])],
            focus: 0,
        }
    }
}

impl Default for AdvisorSetup {
    fn default() -> Self {
        AdvisorSetup::from_suggestions(&[])
    }
}

pub struct App {
    pub projects: Vec<Project>,
    pub cur_project: usize,
    pub cur_session: Option<usize>,
    pub input: String,
    pub cursor: usize,
    pub tick: u64,
    pub show_sidebar: bool,
    pub show_right: bool,
    pub help: bool,
    pub modal_hidden: bool,
    pub quit: bool,
    pub backend: Box<dyn Backend>,
    pub balance: Balance,
    pub snip: Option<SnipState>,
    pub flash: Option<(String, Instant)>,
    /// Real mode checks `/open` paths exist; demo accepts anything.
    pub strict_paths: bool,
    /// Save the workspace list on `/open` and `/close`.
    pub persist: bool,
    pub default_model: String,
    pub mode: Mode,
    /// Configured `(advisor, slave)` models once advisor mode is armed.
    pub advisor: Option<(String, String)>,
    pub setup: Option<AdvisorSetup>,
    pub palette: Option<Palette>,
    pub themes: Option<ThemePick>,
    /// Highlighted row of the `/` suggestion popup.
    pub slash_sel: usize,
    pub panel: Option<Panel>,
    /// dsh workspaces found by the last `/import` scan.
    pub dsh: Vec<destro_core::dsh::DshWorkspace>,
    /// Channel back into the main loop for background work (imports).
    pub tx: Option<std::sync::mpsc::Sender<Msg>>,
    pub history: Vec<String>,
    hist_pos: Option<usize>,
    hist_draft: String,
    last_char: Option<Instant>,
    /// Model names offered by the selected provider.
    pub model_suggestions: Vec<String>,
    tab: Option<TabState>,
    next_rpc: u64,
    auto_selected: bool,
}

impl App {
    pub fn new(projects: Vec<Project>, backend: Box<dyn Backend>) -> App {
        App {
            projects,
            cur_project: 0,
            cur_session: None,
            input: String::new(),
            cursor: 0,
            tick: 0,
            show_sidebar: true,
            show_right: true,
            help: false,
            modal_hidden: false,
            quit: false,
            backend,
            balance: Balance::Unsupported,
            snip: None,
            flash: None,
            strict_paths: false,
            persist: false,
            default_model: "deepseek-v4-pro".into(),
            mode: Mode::Normal,
            advisor: None,
            setup: None,
            palette: None,
            themes: None,
            slash_sel: 0,
            panel: None,
            dsh: vec![],
            tx: None,
            history: vec![],
            hist_pos: None,
            hist_draft: String::new(),
            last_char: None,
            model_suggestions: MODEL_SUGGESTIONS.iter().map(|s| s.to_string()).collect(),
            tab: None,
            next_rpc: 1,
            auto_selected: false,
        }
    }

    pub fn any_running(&self) -> bool {
        self.flash.is_some()
            || self
                .projects
                .iter()
                .any(|p| !p.closed && (p.running_count() > 0 || p.conn == Conn::Starting))
    }

    // ----- navigation -------------------------------------------------------------------

    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        for (pi, p) in self.projects.iter().enumerate() {
            if p.closed {
                continue;
            }
            rows.push(Row::Project(pi));
            if p.collapsed {
                continue;
            }
            fn walk(p: &Project, pi: usize, idx: usize, depth: usize, rows: &mut Vec<Row>) {
                rows.push(Row::Session(pi, idx, depth));
                for c in p.children_of(&p.sessions[idx].id) {
                    walk(p, pi, c, depth + 1, rows);
                }
            }
            for (si, s) in p.sessions.iter().enumerate() {
                let is_root = match &s.parent {
                    None => true,
                    Some(par) => !p.index.contains_key(par),
                };
                if is_root {
                    walk(p, pi, si, 0, &mut rows);
                }
            }
        }
        rows
    }

    pub fn cursor_row(&self) -> usize {
        let want = match self.cur_session {
            Some(s) => Row::Session(self.cur_project, s, usize::MAX),
            None => Row::Project(self.cur_project),
        };
        self.rows()
            .iter()
            .position(|r| match (r, &want) {
                (Row::Project(a), Row::Project(b)) => a == b,
                (Row::Session(p, s, _), Row::Session(wp, ws, _)) => p == wp && s == ws,
                _ => false,
            })
            .unwrap_or(0)
    }

    fn select(&mut self, row: Row) {
        match row {
            Row::Project(p) => {
                self.cur_project = p;
                self.cur_session = None;
            }
            Row::Session(p, s, _) => {
                self.cur_project = p;
                self.cur_session = Some(s);
                self.projects[p].sessions[s].unread = false;
            }
        }
        self.modal_hidden = false;
    }

    fn step(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let n = rows.len() as isize;
        let i = (self.cursor_row() as isize + delta).rem_euclid(n) as usize;
        self.select(rows[i]);
    }

    // ----- protocol ---------------------------------------------------------------------

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Frame { project, line } => self.on_frame(project, &line),
            Msg::Stderr { project, line } => self.projects[project].note(line),
            Msg::Balance(r) => {
                let prev = match std::mem::replace(&mut self.balance, Balance::Loading) {
                    Balance::Ok { info, at } => Some((info, at)),
                    Balance::Err { last, .. } => last,
                    _ => None,
                };
                self.balance = match r {
                    Ok(info) => Balance::Ok {
                        info,
                        at: Instant::now(),
                    },
                    Err(why) => Balance::Err { why, last: prev },
                };
            }
            Msg::Imported {
                cwd,
                new,
                skipped,
                failed,
            } => {
                let mut note = format!("imported {new} chat{}", if new == 1 { "" } else { "s" });
                if skipped > 0 {
                    note.push_str(&format!(" · {skipped} already there"));
                }
                if failed > 0 {
                    note.push_str(&format!(" · {failed} failed"));
                }
                if std::path::Path::new(&cwd).is_dir() {
                    let open = self.projects.iter().position(|p| !p.closed && p.cwd == cwd);
                    if open.is_none() && self.open_workspace(&cwd).is_ok() {
                        note.push_str(" · workspace opened");
                    }
                    note.push_str(" · /resume to continue");
                } else {
                    note.push_str(" · folder no longer exists; /open it to continue");
                }
                self.flash(note);
            }
            Msg::Exited { project, why } => {
                let p = &mut self.projects[project];
                p.conn = Conn::Dead;
                p.note(why);
                for s in p.sessions.iter_mut() {
                    if s.status == Status::Running {
                        s.status = Status::Failed;
                    }
                }
            }
        }
    }

    pub fn on_frame(&mut self, pi: usize, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let method = v["method"].as_str();
        let has_id = v.get("id").is_some_and(|i| !i.is_null());
        match (method, has_id) {
            (Some(m), true) => self.on_request(pi, m, &v),
            (Some(m), false) => self.on_notification(pi, m, &v["params"]),
            (None, true) => {
                if let Some(err) = v.get("error") {
                    let msg = err["message"].as_str().unwrap_or("error").to_string();
                    self.projects[pi].note(format!("rpc error: {msg}"));
                    if v["id"] == 0 {
                        self.projects[pi].conn = Conn::Dead;
                    }
                } else if v["id"] == 0 {
                    self.projects[pi].conn = Conn::Ready;
                }
            }
            _ => {}
        }
    }

    fn on_request(&mut self, pi: usize, method: &str, v: &Value) {
        let params = &v["params"];
        match method {
            "approval/request" => {
                self.projects[pi].approvals.push_back(Approval {
                    rpc_id: v["id"].clone(),
                    session_id: params["sessionId"].as_str().unwrap_or("").into(),
                    tool: params["toolName"].as_str().unwrap_or("?").into(),
                    reason: params["reason"].as_str().map(str::to_string),
                });
                self.modal_hidden = false;
            }
            "user-question/ask" => {
                // not implemented yet: dismissal is explicitly "not approval" in the protocol
                self.backend.send(pi, json!({"jsonrpc":"2.0","id":v["id"],"result":{"cancelled":true}}));
            }
            _ => self.backend.send(pi, json!({"jsonrpc":"2.0","id":v["id"],"error":{"code":-32601,"message":"method not found"}})),
        }
    }

    fn on_notification(&mut self, pi: usize, method: &str, params: &Value) {
        let cur = (self.cur_project, self.cur_session);
        let proj = &mut self.projects[pi];
        match method {
            "session.event" => {
                let sid = params["sessionId"].as_str().unwrap_or("");
                let ev = &params["event"];
                let si = proj.session_mut(sid);
                if ev["type"] == "approval/decided" {
                    proj.approvals.retain(|a| a.session_id != sid);
                }
                proj.sessions[si].apply_event(ev);
                if cur != (pi, Some(si)) {
                    proj.sessions[si].unread = true;
                }
                if !self.auto_selected && pi == self.cur_project && self.cur_session.is_none() {
                    self.auto_selected = true;
                    self.cur_session = Some(si);
                    self.projects[pi].sessions[si].unread = false;
                }
            }
            "session.status" => {
                let sid = params["sessionId"].as_str().unwrap_or("");
                let si = proj.session_mut(sid);
                let running = params["status"] == "running";
                let se = &mut proj.sessions[si];
                if running && se.status != Status::Running {
                    se.started = Some(Instant::now());
                }
                if !running {
                    se.started = None;
                }
                se.status = if running {
                    Status::Running
                } else {
                    Status::Idle
                };
            }
            "subagent.started" => {
                let parent = params["parentSessionId"].as_str().unwrap_or("").to_string();
                let child = params["childSessionId"].as_str().unwrap_or("");
                let ci = proj.session_mut(child);
                proj.sessions[ci].parent = Some(parent);
                proj.sessions[ci].status = Status::Running;
            }
            "subagent.finished" => {
                let child = params["childSessionId"].as_str().unwrap_or("");
                let ci = proj.session_mut(child);
                proj.sessions[ci].status = if params["status"] == "ok" {
                    Status::Idle
                } else {
                    Status::Failed
                };
            }
            _ => {}
        }
    }

    fn rpc(&mut self, project: usize, method: &str, params: Value) {
        let id = self.next_rpc;
        self.next_rpc += 1;
        self.backend.send(
            project,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        );
    }

    pub fn pending_modal(&self) -> Option<(usize, usize)> {
        if self.modal_hidden {
            return None;
        }
        if self.projects[self.cur_project].approvals.is_empty() {
            None
        } else {
            Some((self.cur_project, 0))
        }
    }

    fn answer(&mut self, allow: bool) {
        let p = self.cur_project;
        if let Some(a) = self.projects[p].approvals.pop_front() {
            let outcome = if allow { "allowed-once" } else { "rejected" };
            self.backend.send(
                p,
                json!({"jsonrpc":"2.0","id":a.rpc_id,"result":{"outcome":outcome}}),
            );
        }
    }

    // ----- advisor mode -----------------------------------------------------------------

    pub fn advisor_models(&self) -> Option<(&str, &str)> {
        if self.mode != Mode::Advisor {
            return None;
        }
        self.advisor
            .as_ref()
            .map(|(advisor, slaves)| (advisor.as_str(), slaves.as_str()))
    }

    /// Enter advisor mode; first entry opens the two-model picker.
    pub fn enter_advisor(&mut self) {
        self.mode = Mode::Advisor;
        if self.advisor.is_none() {
            let mut setup = AdvisorSetup::from_suggestions(&self.model_suggestions);
            // Prefer a planner on refuseless and workers on deepseek when both
            // are configured; the fields stay freely editable.
            if let Some(v) = self
                .model_suggestions
                .iter()
                .find(|s| s.starts_with("refuseless/"))
            {
                setup.models[0] = v.clone();
            }
            if let Some(v) = self
                .model_suggestions
                .iter()
                .find(|s| s.starts_with("deepseek/"))
            {
                setup.models[1] = v.clone();
            }
            self.setup = Some(setup);
            self.flash("advisor mode: pick the planner and worker models".into());
        } else {
            self.flash("advisor mode".into());
        }
    }

    pub fn leave_advisor(&mut self) {
        self.mode = Mode::Normal;
        self.setup = None;
        self.flash("normal mode".into());
    }

    pub fn toggle_advisor(&mut self) {
        if self.mode == Mode::Advisor {
            self.leave_advisor();
        } else {
            self.enter_advisor();
        }
    }

    /// Apply models immediately, skipping the picker (used by flags and probes).
    pub fn arm_advisor(&mut self, advisor: &str, slave: &str) {
        let (advisor, slave) = (advisor.trim().to_string(), slave.trim().to_string());
        self.mode = Mode::Advisor;
        self.setup = None;
        self.backend
            .configure_advisor(self.cur_project, &advisor, &slave);
        self.advisor = Some((advisor.clone(), slave.clone()));
        self.flash(format!("advisor {advisor} · slaves {slave}"));
    }

    fn confirm_advisor(&mut self) {
        let Some(setup) = self.setup.clone() else {
            return;
        };
        let advisor = setup.models[0].trim().to_string();
        let slave = setup.models[1].trim().to_string();
        if advisor.is_empty() || slave.is_empty() {
            self.flash("both models are required".into());
            return;
        }
        self.setup = None;
        self.arm_advisor(&advisor, &slave);
    }

    fn advisor_setup_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Tab | KeyCode::Up | KeyCode::Down | KeyCode::BackTab => setup.focus ^= 1,
            KeyCode::Enter => self.confirm_advisor(),
            KeyCode::Esc => {
                self.setup = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                setup.models[setup.focus].pop();
            }
            KeyCode::Char(c) if !ctrl => setup.models[setup.focus].push(c),
            _ => {}
        }
    }

    // ----- commands, palette, themes ----------------------------------------------------

    /// Built-ins, then this workspace's skills and custom commands.
    pub fn entries(&self) -> Vec<Entry> {
        let mut v: Vec<Entry> = COMMANDS
            .iter()
            .map(|c| Entry {
                name: c.name.into(),
                keys: c.keys.into(),
                desc: c.desc.into(),
                arg: c.arg,
                kind: EntryKind::Builtin,
            })
            .collect();
        let pr = &self.projects[self.cur_project];
        for c in &pr.commands {
            v.push(Entry {
                name: format!("/{}", c.name),
                keys: "command".into(),
                desc: c.description.clone(),
                arg: true,
                kind: EntryKind::Command,
            });
        }
        for k in &pr.skills {
            v.push(Entry {
                name: format!("/{}", k.name),
                keys: "skill".into(),
                desc: k.description.clone(),
                arg: true,
                kind: EntryKind::Skill,
            });
        }
        let mut seen = std::collections::HashSet::new();
        v.retain(|e| seen.insert(e.name.clone()));
        v
    }

    /// Entries matching what is typed after a leading `/` (no space yet).
    pub fn slash_matches(&self) -> Vec<Entry> {
        let t = self.input.as_str();
        if !t.starts_with('/') || t.contains(' ') || t.contains('\n') {
            return vec![];
        }
        let q = t.to_lowercase();
        self.entries()
            .into_iter()
            .filter(|c| c.name.to_lowercase().starts_with(&q))
            .collect()
    }

    pub fn palette_matches(&self) -> Vec<Entry> {
        let q = self
            .palette
            .as_ref()
            .map(|p| p.query.trim().trim_start_matches('/').to_lowercase())
            .unwrap_or_default();
        self.entries()
            .into_iter()
            .filter(|c| {
                q.is_empty()
                    || c.name.trim_start_matches('/').to_lowercase().contains(&q)
                    || c.desc.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Put an entry in the prompt. Ones that take an argument wait for it; the
    /// rest complete (Tab) or run (Enter / palette).
    fn run_cmd(&mut self, c: &Entry, run: bool) {
        if c.arg || !run {
            self.input = format!("{} ", c.name);
            if !c.arg {
                self.input.pop();
            }
            self.cursor = self.input.chars().count();
        } else {
            self.input = c.name.clone();
            self.submit();
        }
    }

    fn palette_key(&mut self, k: KeyEvent) {
        let n = self.palette_matches().len();
        let Some(p) = self.palette.as_mut() else {
            return;
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Char('p') if ctrl => self.palette = None,
            KeyCode::Up => p.sel = p.sel.checked_sub(1).unwrap_or(n.saturating_sub(1)),
            KeyCode::Down => p.sel = if n == 0 { 0 } else { (p.sel + 1) % n },
            KeyCode::Backspace => {
                p.query.pop();
                p.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                p.query.push(c);
                p.sel = 0;
            }
            KeyCode::Enter => {
                let sel = p.sel;
                let cmd = self.palette_matches().get(sel).cloned();
                self.palette = None;
                if let Some(c) = cmd {
                    self.run_cmd(&c, true);
                }
            }
            _ => {}
        }
    }

    /// Drop cached transcript lines: they bake the old colours in.
    pub fn refresh_theme(&mut self) {
        for p in self.projects.iter_mut() {
            for s in p.sessions.iter_mut() {
                s.cache.invalidate();
            }
        }
    }

    fn open_theme_picker(&mut self) {
        let cur = theme::index();
        self.themes = Some(ThemePick {
            sel: cur,
            original: cur,
        });
    }

    fn theme_key(&mut self, k: KeyEvent) {
        let n = theme::THEMES.len();
        let Some(tp) = self.themes.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => tp.sel = (tp.sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => tp.sel = (tp.sel + 1) % n,
            KeyCode::Enter => {
                let sel = tp.sel;
                self.themes = None;
                theme::set_index(sel);
                self.refresh_theme();
                crate::config::save_theme(theme::cur().name);
                self.flash(format!("theme: {}", theme::cur().name));
                return;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                let orig = tp.original;
                self.themes = None;
                theme::set_index(orig);
                self.refresh_theme();
                return;
            }
            _ => return,
        }
        let sel = self.themes.as_ref().map(|t| t.sel).unwrap_or(0);
        theme::set_index(sel); // live preview
        self.refresh_theme();
    }

    // ----- input ------------------------------------------------------------------------

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.hist_pos = None;
        if text.is_empty() {
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
            if self.persist {
                crate::config::save_history(&self.history);
            }
        }
        match text.as_str() {
            "/help" => self.help = true,
            "/theme" => self.open_theme_picker(),
            "/model" => self.open_model_panel(),
            _ if text.starts_with("/model ") => {
                let route = text["/model ".len()..].trim().to_string();
                self.apply_model(&route);
            }
            "/resume" => self.open_resume_panel(),
            "/skills" => self.open_skills_panel(),
            "/mcp" => self.open_mcp_panel(),
            "/context" => self.open_context_panel(),
            "/import" => self.open_import_panel(),
            "/export" => self.export_session(),
            _ if text.starts_with("/theme ") => {
                let name = text["/theme ".len()..].trim();
                if theme::set_by_name(name) {
                    self.refresh_theme();
                    crate::config::save_theme(theme::cur().name);
                    self.flash(format!("theme: {}", theme::cur().name));
                } else {
                    self.flash(format!("unknown theme; try: {}", theme::names()));
                }
            }
            "/copy" => {
                let reply = self.cur_session.and_then(|si| {
                    self.projects[self.cur_project].sessions[si]
                        .items
                        .iter()
                        .rev()
                        .find_map(|i| match i {
                            Item::Assistant(t) => Some(t.clone()),
                            _ => None,
                        })
                });
                match reply {
                    Some(t) => self.copy(&t),
                    None => self.flash("no reply to copy yet".into()),
                }
            }
            "/sidebar" => self.show_sidebar = !self.show_sidebar,
            "/panel" => self.show_right = !self.show_right,
            "/quit" | "/exit" => self.quit = true,
            "/new" => self.cur_session = None,
            "/advisor" => self.enter_advisor(),
            "/normal" => self.leave_advisor(),
            "/kill" => self.abort_runtime(),
            "/close" => self.close_workspace(),
            "/snippets" => self.open_snippets(),
            "/open" => {
                self.input = "/open ".into();
                self.cursor = self.input.chars().count();
                self.flash("type a directory path, Tab completes".into());
            }
            _ if text.starts_with("/open ") => {
                if let Err(e) = self.open_workspace(text["/open ".len()..].trim()) {
                    self.flash(format!("open failed: {e}"));
                }
            }
            _ => {
                let text = self.expand_slash(text);
                let p = self.cur_project;
                let si = match self.cur_session {
                    Some(si) => si,
                    None => {
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        let id = format!("tui-{nanos:x}");
                        let si = self.projects[p].session_mut(&id);
                        self.cur_session = Some(si);
                        si
                    }
                };
                let sid = self.projects[p].sessions[si].id.clone();
                self.projects[p].sessions[si].scroll = 0;
                self.rpc(
                    p,
                    "session/query",
                    json!({"sessionId": sid, "contentBlocks": [{"type":"text","text":text}]}),
                );
            }
        }
    }

    fn abort_runtime(&mut self) {
        let p = self.cur_project;
        self.backend.kill(p);
        let proj = &mut self.projects[p];
        proj.conn = Conn::Dead;
        proj.note("runtime aborted by user".into());
        for s in proj.sessions.iter_mut() {
            if s.status == Status::Running {
                s.status = Status::Failed;
                s.items
                    .push_back(Item::Notice("aborted: runtime killed".into()));
            }
        }
        proj.approvals.clear();
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if k.code != KeyCode::Tab {
            self.tab = None;
        }
        if ctrl && matches!(k.code, KeyCode::Char('q') | KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.snip.is_some() {
            self.snippet_key(k);
            return;
        }
        if self.setup.is_some() {
            self.advisor_setup_key(k);
            return;
        }
        if self.panel.is_some() {
            self.panel_key(k);
            return;
        }
        if self.themes.is_some() {
            self.theme_key(k);
            return;
        }
        if self.palette.is_some() {
            self.palette_key(k);
            return;
        }
        if self.pending_modal().is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.answer(true),
                KeyCode::Char('n') | KeyCode::Char('N') => self.answer(false),
                KeyCode::Esc => self.modal_hidden = true,
                _ => {}
            }
            return;
        }
        let page = 10usize;
        // `/` suggestion popup owns the arrows, Tab, Enter and Esc while it is open
        let popup = self.slash_matches();
        if !popup.is_empty() && !alt && !ctrl {
            match k.code {
                KeyCode::Up => {
                    self.slash_sel = self.slash_sel.checked_sub(1).unwrap_or(popup.len() - 1);
                    return;
                }
                KeyCode::Down => {
                    self.slash_sel = (self.slash_sel + 1) % popup.len();
                    return;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    let c = popup[self.slash_sel.min(popup.len() - 1)].clone();
                    let exact = self.input.trim() == c.name;
                    if k.code == KeyCode::Enter && exact && !c.arg {
                        self.submit();
                    } else {
                        self.run_cmd(&c, k.code == KeyCode::Enter);
                    }
                    return;
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.cursor = 0;
                    return;
                }
                _ => {}
            }
        }
        match k.code {
            KeyCode::F(1) => self.help = true,
            KeyCode::Char('p') if ctrl => {
                self.palette = Some(Palette {
                    query: String::new(),
                    sel: 0,
                })
            }
            KeyCode::Char('t') if ctrl => self.open_theme_picker(),
            KeyCode::Up if alt => self.step(-1),
            KeyCode::Down if alt => self.step(1),
            KeyCode::Up if self.input.is_empty() || self.hist_pos.is_some() => self.recall(-1),
            KeyCode::Down if self.hist_pos.is_some() => self.recall(1),
            KeyCode::Esc if self.input.is_empty() => self.interrupt_current(),
            KeyCode::Char('m') if alt => self.open_model_panel(),
            KeyCode::Char('r') if alt => self.open_resume_panel(),
            KeyCode::Left if ctrl => self.word_move(-1),
            KeyCode::Right if ctrl => self.word_move(1),
            KeyCode::Char(c @ '1'..='9') if alt => {
                let n = c as usize - '1' as usize;
                if let Some(i) = (0..self.projects.len())
                    .filter(|&i| !self.projects[i].closed)
                    .nth(n)
                {
                    self.select(Row::Project(i));
                }
            }
            KeyCode::Char('n') if alt => self.cur_session = None,
            KeyCode::Char('a') if alt => self.toggle_advisor(),
            KeyCode::Char('o') if alt => {
                self.input = "/open ".into();
                self.cursor = self.input.chars().count();
            }
            KeyCode::Tab if self.input.starts_with("/open ") => self.complete_path(),
            KeyCode::Char('s') if ctrl => self.open_snippets(),
            KeyCode::Char('y') if ctrl => {
                if let Some(code) = self.cur_snippets().last().map(|s| s.code.clone()) {
                    self.copy(&code);
                } else {
                    self.flash("no snippet to copy yet".into());
                }
            }
            KeyCode::Char('b') if ctrl => self.show_sidebar = !self.show_sidebar,
            KeyCode::Char('g') if ctrl => self.show_right = !self.show_right,
            KeyCode::Char('a') if ctrl => self.modal_hidden = false,
            KeyCode::Char('k') if ctrl => self.abort_runtime(),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::PageUp => self.scroll(page as isize),
            KeyCode::PageDown => self.scroll(-(page as isize)),
            KeyCode::End if self.input.is_empty() => self.scroll(isize::MIN),
            KeyCode::Enter if alt => self.insert('\n'),
            // Windows delivers a paste as key events: an Enter that follows a
            // character within a few ms is part of the pasted text, not a submit.
            KeyCode::Enter
                if self
                    .last_char
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(20)) =>
            {
                self.insert('\n')
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let i = self.byte_at(self.cursor - 1);
                    self.input.remove(i);
                    self.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let i = self.byte_at(self.cursor);
                    self.input.remove(i);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.last_char = Some(Instant::now());
                self.insert(c)
            }
            _ => {}
        }
    }

    fn scroll(&mut self, by: isize) {
        if let Some(si) = self.cur_session {
            let s = &mut self.projects[self.cur_project].sessions[si];
            s.scroll = if by == isize::MIN {
                0
            } else {
                (s.scroll as isize + by).max(0) as usize
            };
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    fn insert(&mut self, c: char) {
        self.slash_sel = 0;
        let i = self.byte_at(self.cursor);
        self.input.insert(i, c);
        self.cursor += 1;
    }

    fn delete_word(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let new: String = chars[..i]
            .iter()
            .chain(chars[self.cursor..].iter())
            .collect();
        self.input = new;
        self.cursor = i;
    }

    // ----- workspaces -------------------------------------------------------------------

    pub fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    /// Expire the status flash; true when the screen needs a redraw.
    pub fn housekeeping(&mut self) -> bool {
        if matches!(self.panel.as_ref().map(|p| p.kind), Some(Kind::Mcp)) {
            let items = self.mcp_items();
            if let Some(p) = self.panel.as_mut() {
                p.refresh(items);
            }
            return true;
        }
        if matches!(&self.flash, Some((_, at)) if at.elapsed() > Duration::from_secs(3)) {
            self.flash = None;
            return true;
        }
        false
    }

    fn base_dir(&self) -> PathBuf {
        let cwd = PathBuf::from(&self.projects[self.cur_project].cwd);
        if cwd.is_dir() {
            cwd
        } else {
            std::env::current_dir().unwrap_or_default()
        }
    }

    fn expand(raw: &str) -> PathBuf {
        let raw = raw.trim().trim_matches('"');
        if (raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\"))
            && let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
        {
            return PathBuf::from(home)
                .join(raw.trim_start_matches('~').trim_start_matches(['/', '\\']));
        }
        PathBuf::from(raw)
    }

    fn resolve_dir(&self, raw: &str) -> Result<String, String> {
        let mut p = Self::expand(raw);
        if p.is_relative() {
            p = self.base_dir().join(p);
        }
        match std::fs::canonicalize(&p) {
            Ok(c) if c.is_dir() => Ok(c.to_string_lossy().trim_start_matches(r"\\?\").to_string()),
            Ok(_) => Err("not a directory".into()),
            Err(_) if !self.strict_paths => Ok(p.to_string_lossy().to_string()),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }

    fn persist_workspaces(&self) {
        if self.persist {
            let paths: Vec<String> = self
                .projects
                .iter()
                .filter(|p| !p.closed)
                .map(|p| p.cwd.clone())
                .collect();
            crate::config::save(&paths);
        }
    }

    pub fn open_workspace(&mut self, raw: &str) -> Result<(), String> {
        let dir = self.resolve_dir(raw)?;
        if let Some(i) = self.projects.iter().position(|p| !p.closed && p.cwd == dir) {
            self.select(Row::Project(i));
            self.flash("already open".into());
            return Ok(());
        }
        let name = std::path::Path::new(&dir)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| dir.clone());
        let idx = self.projects.len();
        self.projects
            .push(Project::new(&name, &dir, &self.default_model));
        if self.strict_paths {
            self.projects[idx].load_conventions();
        }
        self.backend.open(
            idx,
            &ProjectSpec {
                name: name.clone(),
                cwd: dir,
            },
        );
        self.select(Row::Project(idx));
        self.persist_workspaces();
        self.flash(format!("opened {name}"));
        Ok(())
    }

    fn close_workspace(&mut self) {
        let open: Vec<usize> = (0..self.projects.len())
            .filter(|&i| !self.projects[i].closed)
            .collect();
        if open.len() <= 1 {
            self.flash("can't close the last workspace; /open another first".into());
            return;
        }
        let p = self.cur_project;
        self.backend.kill(p);
        self.projects[p].closed = true;
        self.projects[p].approvals.clear();
        let next = open.into_iter().find(|&i| i != p).unwrap_or(0);
        self.select(Row::Project(next));
        self.persist_workspaces();
        self.flash("workspace closed (its runtime was stopped)".into());
    }

    fn complete_path(&mut self) {
        if self.tab.is_none() {
            let partial = self.input["/open ".len()..].to_string();
            let cut = partial.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
            let (dir_str, prefix) = partial.split_at(cut);
            let dir = if dir_str.is_empty() {
                self.base_dir()
            } else {
                let d = Self::expand(dir_str);
                if d.is_relative() {
                    self.base_dir().join(d)
                } else {
                    d
                }
            };
            let sep = if partial.contains('/') {
                '/'
            } else {
                std::path::MAIN_SEPARATOR
            };
            let lp = prefix.to_lowercase();
            let mut cands: Vec<String> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| {
                    n.to_lowercase().starts_with(&lp) && (!lp.is_empty() || !n.starts_with('.'))
                })
                .map(|n| format!("{dir_str}{n}{sep}"))
                .collect();
            cands.sort_by_key(|c| c.to_lowercase());
            if cands.is_empty() {
                return;
            }
            self.tab = Some(TabState {
                cands,
                idx: usize::MAX,
            });
        }
        if let Some(ts) = self.tab.as_mut() {
            ts.idx = ts.idx.wrapping_add(1) % ts.cands.len();
            self.input = format!("/open {}", ts.cands[ts.idx]);
            self.cursor = self.input.chars().count();
        }
    }

    // ----- snippets ---------------------------------------------------------------------

    /// Snippets from the selected session's whole agent tree (root and every
    /// descendant), oldest first, so subagent output is reachable from the parent.
    pub fn cur_snippets(&self) -> Vec<&crate::model::Snippet> {
        let Some(si) = self.cur_session else {
            return vec![];
        };
        let pr = &self.projects[self.cur_project];
        let mut root = si;
        while let Some(&p) = pr.sessions[root]
            .parent
            .as_ref()
            .and_then(|p| pr.index.get(p))
        {
            root = p;
        }
        let mut stack = vec![root];
        let mut all: Vec<&crate::model::Snippet> = vec![];
        while let Some(i) = stack.pop() {
            all.extend(pr.sessions[i].snippets.iter());
            stack.extend(pr.children_of(&pr.sessions[i].id));
        }
        all.sort_by_key(|s| s.id);
        all
    }

    fn open_snippets(&mut self) {
        if self.cur_snippets().is_empty() {
            self.flash("no code snippets in this session yet".into());
        } else {
            self.snip = Some(SnipState { sel: 0, v: 0, h: 0 });
        }
    }

    fn snippet_key(&mut self, k: KeyEvent) {
        let n = self.cur_snippets().len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(st) = self.snip.as_mut() else { return };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.snip = None,
            KeyCode::Char('s') if ctrl => self.snip = None,
            KeyCode::Up | KeyCode::Char('k') => {
                st.sel = st.sel.saturating_sub(1);
                st.v = 0;
                st.h = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                st.sel = (st.sel + 1).min(n.saturating_sub(1));
                st.v = 0;
                st.h = 0;
            }
            KeyCode::PageUp => st.v = st.v.saturating_sub(10),
            KeyCode::PageDown => st.v += 10,
            KeyCode::Left => st.h = st.h.saturating_sub(8),
            KeyCode::Right => st.h += 8,
            KeyCode::Home => {
                st.h = 0;
                st.v = 0;
            }
            KeyCode::Enter | KeyCode::Char('c') | KeyCode::Char('y') => {
                let sel = st.sel;
                let code = self
                    .cur_snippets()
                    .iter()
                    .rev()
                    .nth(sel)
                    .map(|s| s.code.clone());
                if let Some(code) = code {
                    self.copy(&code);
                }
            }
            _ => {}
        }
    }

    /// Copy through the OSC 52 terminal escape, which also works over SSH.
    fn copy(&mut self, text: &str) {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let _ = out.flush();
        self.flash(format!(
            "copied {} bytes (needs OSC 52 support in your terminal)",
            text.len()
        ));
    }

    pub fn input_lines(&self, width: usize) -> Vec<String> {
        wrap(&self.input, width)
    }

    pub fn cursor_xy(&self, width: usize) -> (usize, usize) {
        let prefix: String = self.input.chars().take(self.cursor).collect();
        let lines = wrap(&prefix, width);
        let y = lines.len().saturating_sub(1);
        let x =
            unicode_width::UnicodeWidthStr::width(lines.last().map(String::as_str).unwrap_or(""));
        (x, y)
    }
}

impl App {
    // ----- conventions ------------------------------------------------------------------

    /// Load skills and custom commands for every workspace (real mode only).
    pub fn load_conventions(&mut self) {
        for p in self.projects.iter_mut() {
            p.load_conventions();
        }
    }

    /// `/skill args` becomes a prompt that points the model at the skill;
    /// `/command args` expands the command file. Anything else passes through.
    fn expand_slash(&mut self, text: String) -> String {
        let Some(rest) = text.strip_prefix('/') else {
            return text;
        };
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let pr = &self.projects[self.cur_project];
        if let Some(c) = pr.commands.iter().find(|c| c.name == name) {
            return destro_core::skills::expand_command(c, args.trim());
        }
        if let Some(k) = pr.skills.iter().find(|k| k.name == name) {
            let mut s = format!("Use the `{}` skill (load it with the skill tool).", k.name);
            if !args.trim().is_empty() {
                s.push_str(&format!(" Task: {}", args.trim()));
            }
            return s;
        }
        text
    }

    // ----- history and editing ----------------------------------------------------------

    fn recall(&mut self, dir: isize) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        let next = match (self.hist_pos, dir) {
            (None, -1) => {
                self.hist_draft = std::mem::take(&mut self.input);
                Some(n - 1)
            }
            (Some(i), -1) => Some(i.saturating_sub(1)),
            (Some(i), 1) if i + 1 < n => Some(i + 1),
            (Some(_), 1) => None,
            _ => return,
        };
        self.hist_pos = next;
        self.input = match next {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.hist_draft),
        };
        self.cursor = self.input.chars().count();
    }

    fn word_move(&mut self, dir: isize) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        if dir < 0 {
            while i > 0 && chars[i - 1].is_whitespace() {
                i -= 1;
            }
            while i > 0 && !chars[i - 1].is_whitespace() {
                i -= 1;
            }
        } else {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
        }
        self.cursor = i;
    }

    /// Bracketed paste (where the terminal sends it): insert verbatim.
    pub fn on_paste(&mut self, text: &str) {
        if self.panel.is_some() || self.setup.is_some() {
            return;
        }
        for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            self.insert(c);
        }
    }

    /// Esc with an empty prompt stops the running turn of the current session.
    fn interrupt_current(&mut self) {
        let Some(si) = self.cur_session else { return };
        let p = self.cur_project;
        let s = &self.projects[p].sessions[si];
        if s.status == Status::Running {
            let id = s.id.clone();
            self.backend.interrupt(p, &id);
            self.flash("interrupting…".into());
        }
    }

    // ----- panels -----------------------------------------------------------------------

    fn short(path: &std::path::Path) -> String {
        let s = path.to_string_lossy().to_string();
        match destro_core::context::home_dir() {
            Some(h) => {
                let hs = h.to_string_lossy().to_string();
                match s.strip_prefix(&hs) {
                    Some(rest) => format!("~{rest}"),
                    None => s,
                }
            }
            None => s,
        }
    }

    fn cur_session_id(&self) -> Option<String> {
        self.cur_session
            .map(|si| self.projects[self.cur_project].sessions[si].id.clone())
    }

    fn open_model_panel(&mut self) {
        let cur = self.projects[self.cur_project].model.clone();
        let mut routes: Vec<String> = self.model_suggestions.clone();
        if !routes.contains(&cur) {
            routes.insert(0, cur.clone());
        }
        let items = routes
            .into_iter()
            .map(|r| {
                let it = panel::Item::new(r.clone(), r.clone());
                if r == cur {
                    it.tag("current", Tone::Good)
                } else {
                    it
                }
            })
            .collect();
        let mut p = Panel::new(Kind::Model, "MODEL", items);
        p.footer = "type to filter or enter any provider/model · Enter switch · Esc close".into();
        p.empty = "no match — press Enter to use what you typed".into();
        if let Some(i) = p.items.iter().position(|i| i.id == cur) {
            p.sel = i;
        }
        self.panel = Some(p);
    }

    fn apply_model(&mut self, route: &str) {
        let route = route.trim();
        if route.is_empty() {
            return;
        }
        let p = self.cur_project;
        let sid = self.cur_session_id();
        self.backend.set_model(p, sid.as_deref(), route);
        self.projects[p].model = route.to_string();
        self.default_model = route.to_string();
        self.flash(format!("model: {route}"));
    }

    fn open_skills_panel(&mut self) {
        let pr = &self.projects[self.cur_project];
        let mut items: Vec<panel::Item> = vec![];
        for k in &pr.skills {
            items.push(
                panel::Item::new(format!("/{}", k.name), k.name.clone())
                    .detail(k.description.clone())
                    .tag(k.scope.as_str(), Tone::Muted),
            );
        }
        for c in &pr.commands {
            items.push(
                panel::Item::new(format!("/{}", c.name), format!("/{}", c.name))
                    .detail(c.description.clone())
                    .tag("command", Tone::Good),
            );
        }
        let mut p = Panel::new(Kind::Skills, "SKILLS & COMMANDS", items);
        p.footer = "Enter put it in the prompt · the model can also load skills itself".into();
        p.empty =
            "none found — add SKILL.md folders under .claude/skills or ~/.destro/skills".into();
        self.panel = Some(p);
    }

    fn mcp_items(&self) -> Vec<panel::Item> {
        use destro_core::mcp::McpState;
        self.backend
            .mcp_status(self.cur_project)
            .into_iter()
            .map(|s| {
                let (tag, tone, detail) = match &s.state {
                    McpState::Ready => (
                        "ready",
                        Tone::Good,
                        format!("{} tools · {}", s.tools.len(), s.detail),
                    ),
                    McpState::Connecting => ("connecting", Tone::Warn, s.detail.clone()),
                    McpState::Pending => (
                        "needs approval",
                        Tone::Warn,
                        format!(
                            "from {} · Enter to trust and start · {}",
                            Self::short(&s.source),
                            s.detail
                        ),
                    ),
                    McpState::Disabled => ("disabled", Tone::Muted, s.detail.clone()),
                    McpState::Failed(e) => ("failed", Tone::Bad, e.clone()),
                };
                panel::Item::new(s.name.clone(), s.name.clone())
                    .detail(detail)
                    .tag(tag, tone)
            })
            .collect()
    }

    fn open_mcp_panel(&mut self) {
        let items = self.mcp_items();
        let mut p = Panel::new(Kind::Mcp, "MCP SERVERS", items);
        p.filterable = false;
        p.footer = "Enter approve / toggle · r reconnect · Esc close".into();
        p.empty = "no servers — add .mcp.json to the project or ~/.destro/mcp.json".into();
        self.panel = Some(p);
    }

    fn open_context_panel(&mut self) {
        let cwd = std::path::PathBuf::from(&self.projects[self.cur_project].cwd);
        let files = destro_core::context::load_instructions(&cwd);
        let items = files
            .iter()
            .map(|f| {
                let mut d = format!(
                    "{} lines · {:.1} KB",
                    f.text.lines().count(),
                    f.bytes as f64 / 1024.0
                );
                if f.imports > 0 {
                    d.push_str(&format!(
                        " · {} import{}",
                        f.imports,
                        if f.imports == 1 { "" } else { "s" }
                    ));
                }
                if f.truncated {
                    d.push_str(" · truncated");
                }
                panel::Item::new(f.path.to_string_lossy(), Self::short(&f.path))
                    .detail(d)
                    .tag(
                        f.scope.as_str(),
                        if f.scope == destro_core::context::Scope::User {
                            Tone::Muted
                        } else {
                            Tone::Good
                        },
                    )
            })
            .collect();
        let mut p = Panel::new(Kind::Context, "CONTEXT FILES", items);
        p.filterable = false;
        p.footer = "all of these are in every agent's system prompt · Esc close".into();
        p.empty = "no AGENTS.md or CLAUDE.md found — create AGENTS.md in your project".into();
        self.panel = Some(p);
    }

    fn open_resume_panel(&mut self) {
        let cwd = self.projects[self.cur_project].cwd.clone();
        let open: std::collections::HashSet<String> = self.projects[self.cur_project]
            .sessions
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let items = destro_core::store::list(&cwd)
            .into_iter()
            .map(|m| {
                let mut it =
                    panel::Item::new(m.path.to_string_lossy(), m.title.clone()).detail(format!(
                        "{} · {} turn{}{}",
                        destro_core::context::date_from_ms(m.updated_ms.max(m.created_ms)),
                        m.turns,
                        if m.turns == 1 { "" } else { "s" },
                        if m.model.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", m.model)
                        },
                    ));
                if open.contains(&m.id) {
                    it = it.tag("open", Tone::Good);
                } else if m.origin == "dsh" {
                    it = it.tag("dsh", Tone::Muted);
                }
                it
            })
            .collect();
        let mut p = Panel::new(Kind::Resume, "RESUME A SESSION", items);
        p.footer = "Enter continue it · type to filter · /import brings in dsh chats".into();
        p.empty = "no saved sessions for this workspace yet".into();
        self.panel = Some(p);
    }

    fn open_import_panel(&mut self) {
        let Some(home) = destro_core::dsh::dsh_home() else {
            self.flash("cannot locate the dsh home directory".into());
            return;
        };
        self.dsh = destro_core::dsh::scan(&home);
        let items = self.import_workspace_items();
        let mut p = Panel::new(
            Kind::ImportWorkspaces,
            "IMPORT FROM DSH · WORKSPACES",
            items,
        );
        p.footer = "Enter browse chats · i import every chat in the workspace · Esc close".into();
        p.empty = format!("no dsh chats found under {}", Self::short(&home));
        self.panel = Some(p);
    }

    fn import_workspace_items(&self) -> Vec<panel::Item> {
        self.dsh
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let bytes: u64 = w.sessions.iter().map(|s| s.bytes).sum();
                let open = self.projects.iter().any(|p| !p.closed && p.cwd == w.path);
                let it = panel::Item::new(i.to_string(), w.path.clone()).detail(format!(
                    "{} chat{} · {:.1} MB",
                    w.sessions.len(),
                    if w.sessions.len() == 1 { "" } else { "s" },
                    bytes as f64 / 1_048_576.0
                ));
                if open { it.tag("open", Tone::Good) } else { it }
            })
            .collect()
    }

    fn open_import_sessions(&mut self, ws: usize) {
        let Some(w) = self.dsh.get(ws) else { return };
        let dir = destro_core::store::sessions_dir(&w.path);
        let items = w
            .sessions
            .iter()
            .map(|s| {
                let done = dir
                    .as_ref()
                    .is_some_and(|d| d.join(format!("dsh-{}.jsonl", s.id)).exists());
                let title = if s.title.trim().is_empty() {
                    "(untitled)".to_string()
                } else {
                    s.title.clone()
                };
                let it = panel::Item::new(s.id.clone(), title).detail(format!(
                    "{} · {:.1} MB",
                    destro_core::context::date_from_ms(s.created_ms),
                    s.bytes as f64 / 1_048_576.0
                ));
                if done {
                    it.tag("imported", Tone::Good)
                } else {
                    it
                }
            })
            .collect();
        let mut p = Panel::new(
            Kind::ImportSessions(ws),
            &format!("IMPORT · {}", Self::short(std::path::Path::new(&w.path))),
            items,
        );
        p.multi = true;
        p.footer = "Space tick · Ctrl+A all · Enter import ticked (or this one) · Esc back".into();
        p.empty = "no chats".into();
        self.panel = Some(p);
    }

    /// Convert dsh sessions in the background; reports through `Msg::Imported`.
    fn run_import(&mut self, ws: usize, ids: Vec<String>) {
        let Some(w) = self.dsh.get(ws) else { return };
        let cwd = w.path.clone();
        let chosen: Vec<_> = w
            .sessions
            .iter()
            .filter(|s| ids.contains(&s.id))
            .cloned()
            .collect();
        if chosen.is_empty() {
            return;
        }
        self.flash(format!(
            "importing {} chat{}…",
            chosen.len(),
            if chosen.len() == 1 { "" } else { "s" }
        ));
        let tx = self.tx.clone();
        let work = move || {
            let (mut new, mut skipped, mut failed) = (0, 0, 0);
            for s in &chosen {
                match destro_core::dsh::import_session(s, &cwd, false) {
                    Ok(destro_core::dsh::Imported::New) => new += 1,
                    Ok(destro_core::dsh::Imported::AlreadyThere) => skipped += 1,
                    Err(_) => failed += 1,
                }
            }
            if let Some(tx) = tx {
                let _ = tx.send(Msg::Imported {
                    cwd,
                    new,
                    skipped,
                    failed,
                });
            }
        };
        std::thread::spawn(work);
    }

    fn panel_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(p) = self.panel.as_mut() else { return };
        let kind = p.kind;
        match k.code {
            KeyCode::Esc => {
                let back = matches!(kind, Kind::ImportSessions(_));
                self.panel = None;
                if back {
                    self.panel = {
                        let items = self.import_workspace_items();
                        let mut pn = Panel::new(
                            Kind::ImportWorkspaces,
                            "IMPORT FROM DSH · WORKSPACES",
                            items,
                        );
                        pn.footer =
                            "Enter browse chats · i import every chat in the workspace · Esc close"
                                .into();
                        Some(pn)
                    };
                }
            }
            KeyCode::Up => p.step(-1),
            KeyCode::Down => p.step(1),
            KeyCode::PageUp => p.step(-8),
            KeyCode::PageDown => p.step(8),
            KeyCode::Home => p.sel = 0,
            KeyCode::End => p.sel = p.visible().len().saturating_sub(1),
            KeyCode::Char('a') if ctrl && p.multi => p.mark_all(),
            KeyCode::Char(' ') if p.multi => p.toggle_mark(),
            KeyCode::Char('r') if kind == Kind::Mcp => {
                if let Some(id) = p.current().map(|i| i.id.clone()) {
                    self.backend.mcp_do(self.cur_project, &id, "reconnect");
                }
            }
            KeyCode::Char('i') if kind == Kind::ImportWorkspaces => {
                if let Some(ws) = p.current().and_then(|i| i.id.parse::<usize>().ok()) {
                    let ids: Vec<String> =
                        self.dsh[ws].sessions.iter().map(|s| s.id.clone()).collect();
                    self.panel = None;
                    self.run_import(ws, ids);
                }
            }
            KeyCode::Backspace if p.filterable => {
                p.query.pop();
                p.sel = 0;
            }
            KeyCode::Char(c) if p.filterable && !ctrl => {
                p.query.push(c);
                p.sel = 0;
            }
            KeyCode::Enter => self.panel_enter(),
            _ => {}
        }
    }

    fn panel_enter(&mut self) {
        let Some(p) = self.panel.as_ref() else { return };
        let kind = p.kind;
        let cur = p.current().map(|i| i.id.clone());
        let query = p.query.trim().to_string();
        let marks: Vec<String> = p.marks.iter().cloned().collect();
        match kind {
            Kind::Model => {
                let route = cur.unwrap_or(query);
                self.panel = None;
                self.apply_model(&route);
            }
            Kind::Skills => {
                self.panel = None;
                if let Some(name) = cur {
                    self.input = format!("{name} ");
                    self.cursor = self.input.chars().count();
                }
            }
            Kind::Mcp => {
                let Some(name) = cur else { return };
                let state = self
                    .backend
                    .mcp_status(self.cur_project)
                    .into_iter()
                    .find(|s| s.name == name)
                    .map(|s| s.state);
                use destro_core::mcp::McpState;
                let action = match state {
                    Some(McpState::Pending) => "approve",
                    Some(McpState::Disabled) => "enable",
                    Some(McpState::Ready) => "disable",
                    _ => "reconnect",
                };
                self.backend.mcp_do(self.cur_project, &name, action);
            }
            Kind::Context => {}
            Kind::Resume => {
                self.panel = None;
                if let Some(path) = cur {
                    self.resume_session(&path);
                }
            }
            Kind::ImportWorkspaces => {
                if let Some(ws) = cur.and_then(|i| i.parse::<usize>().ok()) {
                    self.open_import_sessions(ws);
                }
            }
            Kind::ImportSessions(ws) => {
                let ids = if marks.is_empty() {
                    cur.into_iter().collect()
                } else {
                    marks
                };
                self.panel = None;
                self.run_import(ws, ids);
            }
        }
    }

    /// Open a stored session: replay its events into a transcript and hand the
    /// reconstructed conversation to the backend so the agent can continue.
    pub fn resume_session(&mut self, path: &str) {
        let path = std::path::Path::new(path);
        let Some(meta) = destro_core::store::read_meta(path) else {
            self.flash("could not read that session".into());
            return;
        };
        let p = self.cur_project;
        if let Some(&si) = self.projects[p].index.get(&meta.id) {
            self.select(Row::Session(p, si, 0));
            return;
        }
        let events = destro_core::store::load(path);
        let si = self.projects[p].session_mut(&meta.id);
        {
            let s = &mut self.projects[p].sessions[si];
            for ev in &events {
                s.apply_event(ev);
            }
            s.unread = false;
            s.status = Status::Idle;
        }
        let history = destro_core::store::replay_messages(&events);
        self.backend.restore(p, &meta.id, history, &meta.model);
        self.cur_session = Some(si);
        self.flash(format!("resumed: {}", meta.title));
    }

    fn export_session(&mut self) {
        let Some(si) = self.cur_session else {
            self.flash("nothing to export — no session selected".into());
            return;
        };
        let pr = &self.projects[self.cur_project];
        let s = &pr.sessions[si];
        let mut md = format!("# {}\n\n", s.label());
        for it in &s.items {
            match it {
                Item::User(t) => md.push_str(&format!("## You\n\n{t}\n\n")),
                Item::Assistant(t) => md.push_str(&format!("{t}\n\n")),
                Item::ToolCall { name, args } => {
                    md.push_str(&format!("> tool `{name}` {args}\n\n"))
                }
                Item::ToolResult { text, is_error, .. } => md.push_str(&format!(
                    "```{}\n{}\n```\n\n",
                    if *is_error { "error" } else { "" },
                    text
                )),
                Item::Notice(t) => md.push_str(&format!("_{t}_\n\n")),
                Item::Reasoning(_) => {}
            }
        }
        let slug: String = s
            .label()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .split('-')
            .filter(|p| !p.is_empty())
            .take(6)
            .collect::<Vec<_>>()
            .join("-");
        let dir = std::path::PathBuf::from(&pr.cwd);
        let dir = if dir.is_dir() {
            dir
        } else {
            std::env::current_dir().unwrap_or_default()
        };
        let file = dir.join(format!(
            "destro-{}.md",
            if slug.is_empty() { "session" } else { &slug }
        ));
        match std::fs::write(&file, md) {
            Ok(()) => self.flash(format!("saved {}", Self::short(&file))),
            Err(e) => self.flash(format!("export failed: {e}")),
        }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Restored = Vec<(String, Vec<Value>, String)>;
    type Models = Vec<(Option<String>, String)>;

    #[derive(Default, Clone)]
    struct Rec {
        restored: Arc<Mutex<Restored>>,
        models: Arc<Mutex<Models>>,
    }

    impl Backend for Rec {
        fn send(&self, _: usize, _: Value) {}
        fn kill(&self, _: usize) {}
        fn open(&self, _: usize, _: &ProjectSpec) {}
        fn restore(&self, _: usize, session: &str, history: Vec<Value>, model: &str) {
            self.restored
                .lock()
                .unwrap()
                .push((session.into(), history, model.into()));
        }
        fn set_model(&self, _: usize, session: Option<&str>, model: &str) {
            self.models
                .lock()
                .unwrap()
                .push((session.map(str::to_string), model.into()));
        }
    }

    fn app(rec: &Rec) -> App {
        App::new(
            vec![Project::new("p", "/tmp/p", "deepseek-v4-pro")],
            Box::new(rec.clone()),
        )
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn resume_replays_transcript_and_restores_the_conversation() {
        let dir = std::env::temp_dir().join(format!("destro-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut w =
            destro_core::store::SessionWriter::open_at(&path, "/tmp/p", "tui-1", "m1", "destro")
                .unwrap();
        for ev in [
            json!({"type":"user/message","data":{"content":[{"type":"text","text":"hello"}],"source":{"kind":"user"}}}),
            json!({"type":"assistant/message","data":{"message":{"content":[{"type":"text","text":"hi there"}]}}}),
        ] {
            w.append(&ev);
        }
        let rec = Rec::default();
        let mut a = app(&rec);
        a.resume_session(&path.to_string_lossy());
        let s = &a.projects[0].sessions[a.cur_session.unwrap()];
        assert_eq!(s.id, "tui-1");
        assert!(matches!(&s.items[0], Item::User(t) if t == "hello"));
        assert!(matches!(&s.items[1], Item::Assistant(t) if t == "hi there"));
        let got = rec.restored.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.len(), 2);
        assert_eq!(got[0].1[1]["content"], "hi there");
        assert_eq!(got[0].2, "m1");
        // resuming again just selects the open session
        drop(got);
        a.cur_session = None;
        a.resume_session(&path.to_string_lossy());
        assert_eq!(rec.restored.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_command_switches_route_and_remembers_it() {
        let rec = Rec::default();
        let mut a = app(&rec);
        a.input = "/model deepseek/deepseek-v4-flash".into();
        a.submit();
        assert_eq!(a.projects[0].model, "deepseek/deepseek-v4-flash");
        assert_eq!(
            rec.models.lock().unwrap()[0],
            (None, "deepseek/deepseek-v4-flash".to_string())
        );
    }

    #[test]
    fn model_panel_enter_with_typed_route_uses_it() {
        let rec = Rec::default();
        let mut a = app(&rec);
        a.input = "/model".into();
        a.submit();
        assert!(a.panel.is_some());
        for c in "zz/custom".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        a.on_key(key(KeyCode::Enter));
        assert!(a.panel.is_none());
        assert_eq!(a.projects[0].model, "zz/custom");
    }

    #[test]
    fn slash_popup_lists_skills_and_history_recalls() {
        let rec = Rec::default();
        let mut a = app(&rec);
        a.projects[0]
            .commands
            .push(destro_core::skills::CustomCommand {
                name: "deploy".into(),
                description: "ship it".into(),
                path: "x.md".into(),
                scope: destro_core::context::Scope::Project,
                body: "Deploy $ARGUMENTS".into(),
            });
        a.input = "/dep".into();
        let m = a.slash_matches();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "/deploy");
        assert_eq!(a.expand_slash("/deploy prod".into()), "Deploy prod");
        a.history = vec!["first".into(), "second".into()];
        a.input.clear();
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input, "second");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input, "first");
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.input, "");
    }
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
