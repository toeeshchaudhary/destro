//! Rendering. Monochrome on purpose: weight, dim, reverse and a few glyphs carry
//! all the meaning, so it reads the same on any terminal theme.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Mode, Row};
use crate::balance::Balance;
use crate::highlight::{self, Highlighter, expand_tabs};
use crate::md;
use crate::model::{Conn, Item, Project, Session, Status, truncate};

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Wrapped transcript lines for one session, built incrementally: items are
/// append-only so only new items are laid out; a width change rebuilds.
#[derive(Default)]
pub struct LineCache {
    width: u16,
    rendered: usize,
    dropped: usize,
    pub lines: Vec<Line<'static>>,
}

impl LineCache {
    pub fn invalidate(&mut self) {
        self.width = 0;
        self.rendered = 0;
        self.lines.clear();
    }

    fn sync(&mut self, s: &Session, width: u16) {
        if self.width != width || self.dropped != s.dropped {
            self.invalidate();
            self.width = width;
            self.dropped = s.dropped;
        }
        for item in s.items.iter().skip(self.rendered) {
            layout_item(item, width as usize, &mut self.lines);
        }
        self.rendered = s.items.len();
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}
fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}
fn rev() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = vec![];
    for para in text.split('\n') {
        let mut line = String::new();
        let mut lw = 0usize;
        for word in para.split(' ') {
            let ww = word.width();
            if ww > width {
                // hard-split an over-long token
                if lw > 0 {
                    out.push(std::mem::take(&mut line));
                    lw = 0;
                }
                for ch in word.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if lw + cw > width {
                        out.push(std::mem::take(&mut line));
                        lw = 0;
                    }
                    line.push(ch);
                    lw += cw;
                }
                continue;
            }
            let sep = if lw == 0 { 0 } else { 1 };
            if lw + sep + ww > width {
                out.push(std::mem::take(&mut line));
                lw = 0;
            }
            if lw > 0 {
                line.push(' ');
                lw += 1;
            }
            line.push_str(word);
            lw += ww;
        }
        out.push(line);
    }
    out
}

fn layout_item(item: &Item, width: usize, out: &mut Vec<Line<'static>>) {
    let w = width.saturating_sub(3).max(10);
    match item {
        Item::User(t) => {
            for (i, l) in wrap(t, w).into_iter().enumerate() {
                let mark = if i == 0 { "▌ " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(mark, bold()),
                    Span::styled(l, bold()),
                ]));
            }
            out.push(Line::raw(""));
        }
        Item::Assistant(t) => {
            md::layout(t, width, out);
            out.push(Line::raw(""));
        }
        Item::Reasoning(t) => {
            let lines = wrap(t, w.saturating_sub(2));
            let n = lines.len();
            for (i, l) in lines.into_iter().take(3).enumerate() {
                let mark = if i == 0 { "· " } else { "  " };
                out.push(Line::styled(
                    format!("  {mark}{l}"),
                    dim().add_modifier(Modifier::ITALIC),
                ));
            }
            if n > 3 {
                out.push(Line::styled(
                    format!("    … {} more lines of reasoning", n - 3),
                    dim(),
                ));
            }
        }
        Item::ToolCall { name, args } => {
            let a = args.replace('\n', " ");
            let text = truncate(&format!("▸ {name}  {a}"), width.saturating_sub(4));
            out.push(Line::from(vec![Span::raw("  "), Span::styled(text, dim())]));
        }
        Item::ToolResult {
            is_error,
            text,
            lang,
        } => {
            if let Some(l) = lang {
                let max = if l == "diff" { 14 } else { 8 };
                md::code_block(l, text, width, "    ", max, out);
                out.push(Line::raw(""));
                return;
            }
            let lines: Vec<&str> = text.lines().collect();
            for (i, l) in lines.iter().take(4).enumerate() {
                let l = truncate(l, width.saturating_sub(8));
                let mark = if i == 0 {
                    if *is_error { "✗ " } else { "↳ " }
                } else {
                    "  "
                };
                let st = if *is_error { rev() } else { dim() };
                out.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{mark}{l}"), st),
                ]));
            }
            if lines.len() > 4 {
                out.push(Line::styled(
                    format!("      … {} more lines", lines.len() - 4),
                    dim(),
                ));
            }
            out.push(Line::raw(""));
        }
        Item::Notice(t) => {
            out.push(Line::styled(format!("  ── {t} ──"), dim()));
            out.push(Line::raw(""));
        }
    }
}

fn status_glyph(st: Status, tick: u64) -> Span<'static> {
    match st {
        Status::Running => Span::styled(SPIN[(tick as usize) % SPIN.len()], bold()),
        Status::Idle => Span::styled("○", dim()),
        Status::Failed => Span::styled("✗", rev()),
    }
}

fn k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn gauge(ratio: f64, width: usize) -> String {
    let filled = (ratio * width as f64).round() as usize;
    format!(
        "{}{}",
        "█".repeat(filled.min(width)),
        "░".repeat(width - filled.min(width))
    )
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let show_side = app.show_sidebar && area.width >= 70;
    let show_right = app.show_right && area.width >= 110;
    let mut cons = vec![];
    if show_side {
        cons.push(Constraint::Length(30));
    }
    cons.push(Constraint::Min(30));
    if show_right {
        cons.push(Constraint::Length(34));
    }
    let cols = Layout::horizontal(cons).split(area);
    let mut i = 0;
    if show_side {
        draw_sidebar(f, app, cols[i]);
        i += 1;
    }
    draw_main(f, app, cols[i]);
    i += 1;
    if show_right {
        draw_right(f, app, cols[i]);
    }
    if app.snip.is_some() {
        draw_snippets(f, app, area);
    }
    if app.help {
        draw_help(f, area);
    } else if let Some(p) = app.pending_modal() {
        draw_approval(f, app, p, area);
    }
    if app.setup.is_some() {
        draw_advisor_setup(f, app, area);
    }
}

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::RIGHT).border_style(dim());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(" DESTRO", bold()),
            Span::styled("  harness tui", dim()),
        ]),
        Line::raw(""),
    ];
    let w = inner.width as usize;
    for (ri, row) in app.rows().iter().enumerate() {
        let selected = ri == app.cursor_row();
        match *row {
            Row::Project(p) => {
                let pr = &app.projects[p];
                let run = pr.running_count();
                let conn = match pr.conn {
                    Conn::Starting => " …",
                    Conn::Ready => "",
                    Conn::Dead => " ✗",
                };
                let caret = if pr.collapsed { "▸" } else { "▾" };
                let badge = if !pr.approvals.is_empty() {
                    format!(" !{}", pr.approvals.len())
                } else if run > 0 {
                    format!(" ●{run}")
                } else {
                    String::new()
                };
                let name = truncate(
                    &pr.name.to_uppercase(),
                    w.saturating_sub(5 + badge.chars().count()),
                );
                let st = if selected { rev() } else { bold() };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {caret} {name}{conn}"), st),
                    Span::styled(badge, if selected { rev() } else { bold() }),
                ]));
            }
            Row::Session(p, s, depth) => {
                let pr = &app.projects[p];
                let se = &pr.sessions[s];
                let pend = pr.approvals.iter().any(|a| a.session_id == se.id);
                let pad = "  ".repeat(depth + 1);
                let mark = if pend {
                    Span::styled("!", rev())
                } else if se.unread {
                    Span::styled("•", bold())
                } else {
                    Span::raw(" ")
                };
                let label = truncate(&se.label(), w.saturating_sub(pad.len() + 5));
                let st = if selected {
                    rev()
                } else if se.status == Status::Running {
                    Style::default()
                } else {
                    dim()
                };
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    mark,
                    Span::raw(pad),
                    status_glyph(se.status, app.tick),
                    Span::styled(format!(" {label}"), st),
                ]));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let input_lines = app.input_lines(area.width.saturating_sub(4) as usize);
    let ih = (input_lines.len() as u16).clamp(1, 5);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(ih + 1),
        Constraint::Length(1),
    ])
    .split(area);

    // header
    let (cp, cs) = (app.cur_project, app.cur_session);
    let pr = &app.projects[cp];
    let mut head: Vec<Span> = Vec::new();
    if let Some((advisor, slaves)) = app.advisor_models() {
        head.push(Span::styled(" ADVISOR ", rev()));
        head.push(Span::styled(format!("{advisor} ▸ {slaves}  "), dim()));
    }
    head.push(Span::styled(format!(" {} ", pr.name), bold()));
    if let Some(si) = cs {
        let se = &pr.sessions[si];
        head.push(Span::styled(
            format!("/ {}", truncate(&se.label(), 40)),
            Style::default(),
        ));
        head.push(Span::styled(
            format!("   turn {}  ·  {}", se.turn, pr.model),
            dim(),
        ));
        if se.scroll > 0 {
            head.push(Span::styled(
                format!("   ↑ scrolled {} lines (End to follow)", se.scroll),
                rev(),
            ));
        }
        if se.dropped > 0 {
            head.push(Span::styled(
                format!("   {} older items evicted from view", se.dropped),
                dim(),
            ));
        }
    }
    let bal = balance_header(&app.balance);
    let bw = bal.as_ref().map(|s| s.content.width() as u16).unwrap_or(0);
    let left = Rect {
        width: rows[0].width.saturating_sub(bw + 1),
        ..rows[0]
    };
    f.render_widget(Paragraph::new(Line::from(head)), left);
    if let Some(span) = bal {
        let r = Rect {
            x: rows[0].x + rows[0].width - bw,
            width: bw,
            ..rows[0]
        };
        f.render_widget(Paragraph::new(span), r);
    }

    // transcript
    let body = rows[1];
    match cs {
        None => draw_project_overview(f, &app.projects[cp], body),
        Some(si) => {
            let width = body.width.saturating_sub(2);
            let se = &mut app.projects[cp].sessions[si];
            let mut cache = std::mem::take(&mut se.cache);
            cache.sync(se, width);
            let h = body.height as usize;
            let total = cache.lines.len();
            let max_scroll = total.saturating_sub(h);
            se.scroll = se.scroll.min(max_scroll);
            let end = total - se.scroll;
            let start = end.saturating_sub(h);
            let slice: Vec<Line> = cache.lines[start..end].to_vec();
            se.cache = cache;
            let inner = Rect {
                x: body.x + 1,
                y: body.y,
                width: body.width.saturating_sub(1),
                height: body.height,
            };
            f.render_widget(Paragraph::new(slice), inner);
        }
    }

    // input
    let block = Block::default().borders(Borders::TOP).border_style(dim());
    let inner = block.inner(rows[2]);
    f.render_widget(block, rows[2]);
    let shown: Vec<Line> = input_lines
        .iter()
        .take(ih as usize)
        .enumerate()
        .map(|(i, l)| {
            Line::from(vec![
                Span::styled(if i == 0 { " › " } else { "   " }, bold()),
                Span::raw(l.clone()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(shown), inner);
    let (cx, cy) = app.cursor_xy(area.width.saturating_sub(4) as usize);
    if app.pending_modal().is_none() && !app.help {
        f.set_cursor_position((inner.x + 3 + cx as u16, inner.y + (cy as u16).min(ih - 1)));
    }

    // hint
    let hint = " Enter send · Alt+↑↓ switch · Alt+A advisor · Alt+O open workspace · Ctrl+S snippets · F1 help · Ctrl+Q quit";
    let line = match &app.flash {
        Some((m, _)) => Span::styled(format!(" {m}"), bold()),
        None => Span::styled(hint, dim()),
    };
    f.render_widget(Paragraph::new(line), rows[3]);
}

fn draw_project_overview(f: &mut Frame, pr: &Project, area: Rect) {
    let mut l = vec![
        Line::raw(""),
        Line::styled(format!("  {}", pr.name.to_uppercase()), bold()),
        Line::styled(format!("  {}", pr.cwd), dim()),
        Line::styled(
            format!(
                "  model {}  ·  {} sessions  ·  {} running",
                pr.model,
                pr.sessions.len(),
                pr.running_count()
            ),
            dim(),
        ),
        Line::raw(""),
        Line::raw("  No session selected. Type a prompt to start one in this project."),
        Line::raw(""),
    ];
    for s in pr
        .log
        .iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        l.push(Line::styled(
            format!(
                "  runtime: {}",
                truncate(s, area.width.saturating_sub(14) as usize)
            ),
            dim(),
        ));
    }
    f.render_widget(Paragraph::new(l), area);
}

fn draw_right(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::LEFT).border_style(dim());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let pr = &app.projects[app.cur_project];
    let w = inner.width as usize;
    let mut l: Vec<Line> = vec![Line::raw("")];
    let Some(si) = app.cur_session else {
        l.push(Line::styled(" RUNTIME", bold()));
        l.push(Line::styled(
            match pr.conn {
                Conn::Starting => " starting…",
                Conn::Ready => " ready",
                Conn::Dead => " not running",
            },
            dim(),
        ));
        f.render_widget(Paragraph::new(l), inner);
        return;
    };
    // root of the selected tree
    let mut root = si;
    while let Some(p) = pr.sessions[root]
        .parent
        .as_ref()
        .and_then(|p| pr.index.get(p))
    {
        root = *p;
    }
    let kids = pr.children_of(&pr.sessions[root].id);
    let run = kids
        .iter()
        .filter(|&&c| pr.sessions[c].status == Status::Running)
        .count();
    if app.mode == Mode::Advisor {
        let (advisor, slaves) = app.advisor_models().unwrap_or(("?", "?"));
        let reported = kids
            .iter()
            .filter(|&&c| pr.sessions[c].status != Status::Running)
            .count();
        l.push(Line::styled(" ┏━ ADVISOR ━━━━━━━━━━━━━", bold()));
        l.push(Line::from(vec![
            Span::styled(" ┃ ", dim()),
            Span::styled("plans  ", dim()),
            Span::styled(truncate(advisor, w.saturating_sub(10)), bold()),
        ]));
        l.push(Line::from(vec![
            Span::styled(" ┃ ", dim()),
            Span::styled("slaves ", dim()),
            Span::styled(truncate(slaves, w.saturating_sub(10)), bold()),
        ]));
        l.push(Line::styled(
            format!(" ┃ {}/{} workers reported", reported, kids.len()),
            dim(),
        ));
        l.push(Line::styled(" ┗━━━━━━━━━━━━━━━━━━━━━", bold()));
        l.push(Line::raw(""));
    }
    l.push(Line::from(vec![
        Span::styled(" AGENTS", bold()),
        Span::styled(
            format!(
                "   {} running / {}",
                run + (pr.sessions[root].status == Status::Running) as usize,
                kids.len() + 1
            ),
            dim(),
        ),
    ]));
    let mut push_agent = |s: &Session, name: &str, sel: bool| {
        let st = if sel { rev() } else { Style::default() };
        l.push(Line::from(vec![
            Span::raw(" "),
            status_glyph(s.status, app.tick),
            Span::styled(format!(" {}", truncate(name, w.saturating_sub(4))), st),
        ]));
        if let Some(t) = &s.last_tool {
            if s.status == Status::Running {
                l.push(Line::styled(
                    format!("   ▸ {}", truncate(t, w.saturating_sub(6))),
                    dim(),
                ));
            }
        }
    };
    push_agent(&pr.sessions[root], "main", root == si);
    for &c in &kids {
        let name = pr.sessions[c].label();
        push_agent(&pr.sessions[c], &name, c == si);
    }
    l.push(Line::raw(""));

    let se = &pr.sessions[si];
    l.push(Line::styled(" CONTEXT", bold()));
    match se.ctx_ratio() {
        Some(r) => {
            l.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    gauge(r, w.saturating_sub(8)),
                    if r > 0.85 { rev() } else { Style::default() },
                ),
                Span::raw(format!(" {:>3}%", (r * 100.0).round() as u32)),
            ]));
            l.push(Line::styled(
                format!(
                    " {} / {}",
                    k(se.usage.last_context),
                    k(se.context_window.unwrap_or(0))
                ),
                dim(),
            ));
        }
        None => l.push(Line::styled(
            format!(" {} tokens", k(se.usage.last_context)),
            dim(),
        )),
    }
    l.push(Line::raw(""));
    let (mut ti, mut to, mut tc) = (0, 0, 0);
    for s in std::iter::once(&pr.sessions[root]).chain(kids.iter().map(|&c| &pr.sessions[c])) {
        ti += s.usage.input;
        to += s.usage.output;
        tc += s.usage.cache_read;
    }
    l.push(Line::styled(" TOKENS (tree)", bold()));
    l.push(Line::raw(format!(" in     {:>8}", k(ti))));
    l.push(Line::raw(format!(" out    {:>8}", k(to))));
    l.push(Line::raw(format!(" cached {:>8}", k(tc))));
    if ti + tc > 0 {
        l.push(Line::styled(
            format!(" hit    {:>7}%", tc * 100 / (ti + tc)),
            dim(),
        ));
    }
    l.push(Line::raw(""));
    l.extend(balance_lines(&app.balance));
    f.render_widget(Paragraph::new(l), inner);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

fn draw_advisor_setup(f: &mut Frame, app: &App, area: Rect) {
    use crate::app::MODEL_SUGGESTIONS;
    let Some(setup) = &app.setup else {
        return;
    };
    let r = centered(area, 78, 14);
    f.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(" ADVISOR MODE ")
        .title_alignment(ratatui::layout::Alignment::Center);
    let inner = block.inner(r);
    f.render_widget(block, r);

    let mut lines: Vec<Line> = vec![
        Line::styled(
            " advisor plans and judges · subagents execute · advisor synthesises",
            dim(),
        ),
        Line::raw(""),
        Line::styled(
            "   task ──▶ advisor ──▶ workers ──▶ advisor ──▶ result",
            bold(),
        ),
        Line::raw(""),
    ];
    let labels = ["advisor model", "slave models "];
    for (i, label) in labels.iter().enumerate() {
        let focused = setup.focus == i;
        let value = &setup.models[i];
        let mut spans = vec![
            Span::styled(format!(" {label} "), if focused { rev() } else { dim() }),
            Span::raw("  "),
        ];
        if focused {
            spans.push(Span::styled(
                format!(" {value} "),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::styled("▏", bold()));
        } else {
            spans.push(Span::styled(format!(" {value} "), bold()));
        }
        lines.push(Line::from(spans));
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        format!(" suggestions: {}", MODEL_SUGGESTIONS.join(" · ")),
        dim(),
    ));
    lines.push(Line::styled(
        " Tab switch field · type to replace · Enter start · Esc back to normal",
        dim(),
    ));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_approval(f: &mut Frame, app: &App, (p, a): (usize, usize), area: Rect) {
    let ap = &app.projects[p].approvals[a];
    let who = app.projects[p]
        .index
        .get(&ap.session_id)
        .map(|&i| app.projects[p].sessions[i].label())
        .unwrap_or_default();
    let r = centered(area, 64, 9);
    f.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" APPROVAL ", rev()));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut l = vec![
        Line::from(vec![
            Span::styled(format!(" {} ", app.projects[p].name), bold()),
            Span::styled(format!("/ agent {who}"), dim()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw(" tool   "),
            Span::styled(ap.tool.clone(), bold()),
        ]),
    ];
    for (i, line) in wrap(
        ap.reason.as_deref().unwrap_or("(no reason given)"),
        inner.width as usize - 10,
    )
    .into_iter()
    .take(2)
    .enumerate()
    {
        l.push(Line::raw(format!(
            " {} {}",
            if i == 0 { "reason " } else { "       " },
            line
        )));
    }
    l.push(Line::raw(""));
    l.push(Line::styled(
        " [y] allow once    [n] reject    [Esc] decide later",
        bold(),
    ));
    f.render_widget(Paragraph::new(l), inner);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let r = centered(area, 72, 25);
    f.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" KEYS ", rev()));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let rows = [
        ("Enter", "send prompt (slash commands go to the harness)"),
        ("Alt+Enter", "newline in prompt"),
        ("Alt+↑ / Alt+↓", "previous / next project or session"),
        ("Alt+1..9", "jump to project"),
        ("Alt+N", "new session in current project"),
        (
            "Alt+A  /advisor",
            "advisor mode: pick planner + worker models, then delegate",
        ),
        ("/normal", "leave advisor mode"),
        (
            "/open <path>",
            "add a workspace (Alt+O prefills, Tab completes dirs)",
        ),
        ("/close", "close current workspace and stop its runtime"),
        (
            "Ctrl+S",
            "snippet viewer: browse, pan, copy code from this session",
        ),
        ("Ctrl+Y", "copy the newest snippet"),
        ("PgUp / PgDn / End", "scroll transcript / follow tail"),
        ("Ctrl+B / Ctrl+G", "toggle sidebar / agents panel"),
        ("Ctrl+A", "reopen pending approval"),
        (
            "Ctrl+K",
            "abort runtime of current project (SDK has no cancel)",
        ),
        ("Ctrl+U / Ctrl+W", "clear prompt / delete word"),
        ("F1", "this help"),
        ("Ctrl+Q", "quit"),
    ];
    let mut l: Vec<Line> = rows
        .iter()
        .map(|(a, b)| {
            Line::from(vec![
                Span::styled(format!(" {a:<20}"), bold()),
                Span::raw(b.to_string()),
            ])
        })
        .collect();
    l.push(Line::raw(""));
    l.push(Line::styled(
        " ● running  ○ idle  ✗ failed  ! needs you  • unread",
        dim(),
    ));
    f.render_widget(Paragraph::new(l), inner);
}

fn balance_header(b: &Balance) -> Option<Span<'static>> {
    match b {
        Balance::Unsupported => None,
        Balance::NoKey => Some(Span::styled(" balance: no API key ", dim())),
        Balance::Loading => Some(Span::styled(" balance … ", dim())),
        _ => match b.short() {
            Some(s) if b.is_low() => Some(Span::styled(format!(" {s} LOW "), rev())),
            Some(s) => Some(Span::styled(format!(" {s} "), bold())),
            None => Some(Span::styled(" balance ✗ ", dim())),
        },
    }
}

fn balance_lines(b: &Balance) -> Vec<Line<'static>> {
    let mut l = vec![Line::styled(" BALANCE", bold())];
    let (info, at, err) = match b {
        Balance::Unsupported => {
            l.push(Line::styled(" n/a for this provider", dim()));
            return l;
        }
        Balance::NoKey => {
            l.push(Line::styled(" set DEEPSEEK_API_KEY", dim()));
            l.push(Line::styled(" (env or .env) to show it", dim()));
            return l;
        }
        Balance::Loading => {
            l.push(Line::styled(" loading…", dim()));
            return l;
        }
        Balance::Ok { info, at } => (Some(info), Some(*at), None),
        Balance::Err { why, last } => (
            last.as_ref().map(|(i, _)| i),
            last.as_ref().map(|(_, a)| *a),
            Some(why.clone()),
        ),
    };
    if let Some(info) = info {
        for w in &info.wallets {
            let sym = Balance::sym(&w.currency);
            l.push(Line::from(vec![
                Span::raw(format!(" {} ", w.currency)),
                Span::styled(
                    format!("{sym}{}", w.total),
                    if b.is_low() { rev() } else { bold() },
                ),
            ]));
            l.push(Line::styled(
                format!("   topped up {sym}{}", w.topped_up),
                dim(),
            ));
            l.push(Line::styled(
                format!("   granted   {sym}{}", w.granted),
                dim(),
            ));
        }
        if !info.available {
            l.push(Line::styled(" account not available", rev()));
        }
    }
    if let Some(at) = at {
        l.push(Line::styled(
            format!(" updated {}s ago", at.elapsed().as_secs()),
            dim(),
        ));
    }
    if let Some(e) = err {
        l.push(Line::styled(format!(" ✗ {}", truncate(&e, 28)), rev()));
    }
    l
}

fn draw_snippets(f: &mut Frame, app: &mut App, area: Rect) {
    let r = area;
    f.render_widget(Clear, r);
    let n = app.cur_snippets().len();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(format!(" SNIPPETS · {n} "), rev()));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let cols = Layout::horizontal([Constraint::Length(34), Constraint::Min(20)]).split(inner);
    let rows = Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).split(cols[1]);
    let list_h = cols[0].height as usize;

    // clamp scrolling against the selected snippet
    let sel = app
        .snip
        .as_ref()
        .map(|s| s.sel)
        .unwrap_or(0)
        .min(n.saturating_sub(1));
    let total_lines = app
        .cur_snippets()
        .iter()
        .rev()
        .nth(sel)
        .map(|s| s.code.split('\n').count())
        .unwrap_or(0);
    let body_h = rows[0].height.saturating_sub(2) as usize;
    if let Some(st) = app.snip.as_mut() {
        st.sel = sel;
        st.v = st.v.min(total_lines.saturating_sub(body_h.max(1)));
    }
    let (v, h) = app.snip.as_ref().map(|s| (s.v, s.h)).unwrap_or((0, 0));

    // list, newest first
    let start = sel.saturating_sub(list_h.saturating_sub(1));
    let mut list: Vec<Line> = vec![];
    for (i, s) in app
        .cur_snippets()
        .iter()
        .rev()
        .enumerate()
        .skip(start)
        .take(list_h)
    {
        let label = truncate(&s.title, 20);
        let meta = format!(
            "{} · {}L",
            if s.lang.is_empty() { "text" } else { &s.lang },
            s.code.split('\n').count()
        );
        let text = format!(" {label:<20} {}", truncate(&meta, 11));
        list.push(Line::styled(
            truncate(&text, cols[0].width as usize - 1),
            if i == sel { rev() } else { Style::default() },
        ));
    }
    f.render_widget(
        Paragraph::new(list).block(Block::default().borders(Borders::RIGHT).border_style(dim())),
        cols[0],
    );

    // preview
    let snips = app.cur_snippets();
    let Some(s) = snips.iter().rev().nth(sel) else {
        return;
    };
    let who = app.projects[app.cur_project]
        .index
        .get(&s.from)
        .map(|&i| app.projects[app.cur_project].sessions[i].label())
        .unwrap_or_default();
    let digits = total_lines.to_string().len().max(2);
    let body_w = (rows[0].width as usize).saturating_sub(digits + 4);
    let mut out: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(format!(" {}", s.title), bold()),
            Span::styled(
                format!(
                    "   {} · {} lines · from {}",
                    if s.lang.is_empty() { "text" } else { &s.lang },
                    total_lines,
                    truncate(&who, 40)
                ),
                dim(),
            ),
        ]),
        Line::raw(""),
    ];
    let mut hl = Highlighter::new(&s.lang);
    for (i, raw) in s.code.split('\n').enumerate().take(v + body_h) {
        let toks = hl.line(&expand_tabs(raw));
        if i < v {
            continue;
        }
        let mut row = vec![
            Span::styled(format!(" {:>digits$} ", i + 1), dim()),
            Span::styled("│ ", dim()),
        ];
        row.extend(highlight::to_spans(&toks, h, body_w));
        out.push(Line::from(row));
    }
    f.render_widget(Paragraph::new(out), rows[0]);
    f.render_widget(
        Paragraph::new(Span::styled(
            " ↑↓ select · PgUp/PgDn scroll · ←→ pan · c copy · Esc close",
            dim(),
        )),
        rows[1],
    );
}
