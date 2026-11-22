//! Application state, protocol dispatch and key handling.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::backend::{Backend, Msg, ProjectSpec};
use crate::balance::Balance;
use crate::model::{Approval, Conn, Item, Project, Status};
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

pub const COMMANDS: [Cmd; 13] = [
    Cmd {
        name: "/new",
        keys: "Alt+N",
        desc: "start a new session in this workspace",
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
        keys: "Ctrl+Y",
        desc: "copy the newest code snippet",
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
                proj.sessions[si].status = if params["status"] == "running" {
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
            self.setup = Some(AdvisorSetup::from_suggestions(&self.model_suggestions));
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

    /// Commands matching what is typed after a leading `/` (no space yet).
    pub fn slash_matches(&self) -> Vec<&'static Cmd> {
        let t = self.input.as_str();
        if !t.starts_with('/') || t.contains(' ') || t.contains('\n') {
            return vec![];
        }
        let q = t.to_lowercase();
        COMMANDS.iter().filter(|c| c.name.starts_with(&q)).collect()
    }

    pub fn palette_matches(&self) -> Vec<&'static Cmd> {
        let q = self
            .palette
            .as_ref()
            .map(|p| p.query.trim().trim_start_matches('/').to_lowercase())
            .unwrap_or_default();
        COMMANDS
            .iter()
            .filter(|c| {
                q.is_empty()
                    || c.name.trim_start_matches('/').contains(&q)
                    || c.desc.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Put a command in the prompt. Arg-taking commands wait for their argument;
    /// others complete (Tab) or run (Enter / palette).
    fn run_cmd(&mut self, c: &Cmd, run: bool) {
        if c.arg || !run {
            self.input = format!("{} ", c.name);
            if !c.arg {
                self.input.pop();
            }
            self.cursor = self.input.chars().count();
        } else {
            self.input = c.name.to_string();
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
                let cmd = self.palette_matches().get(sel).copied();
                self.palette = None;
                if let Some(c) = cmd {
                    self.run_cmd(c, true);
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
        if text.is_empty() {
            return;
        }
        match text.as_str() {
            "/help" => self.help = true,
            "/theme" => self.open_theme_picker(),
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
                if let Some(code) = self.cur_snippets().last().map(|s| s.code.clone()) {
                    self.copy(&code);
                } else {
                    self.flash("no snippet to copy yet".into());
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
                    let c = popup[self.slash_sel.min(popup.len() - 1)];
                    let exact = self.input.trim() == c.name;
                    if k.code == KeyCode::Enter && exact && !c.arg {
                        self.submit();
                    } else {
                        self.run_cmd(c, k.code == KeyCode::Enter);
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
            KeyCode::Up if self.input.is_empty() => self.scroll(3),
            KeyCode::Down if self.input.is_empty() => self.scroll(-3),
            KeyCode::Up if alt => self.step(-1),
            KeyCode::Down if alt => self.step(1),
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
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c),
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
