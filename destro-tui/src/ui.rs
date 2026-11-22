//! Rendering. Every colour comes from the active [`theme`]; glyphs and weight
//! carry the structure so even the `mono` theme stays readable.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Cmd, Mode, Row};
use crate::balance::Balance;
use crate::highlight::{self, Highlighter, expand_tabs};
use crate::md;
use crate::model::{Conn, Item, Project, Session, Status, truncate};
use crate::theme::{self, THEMES};

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOGO: [&str; 3] = [
    "╺┳┓┏━╸┏━┓╺┳╸┏━┓┏━┓",
    " ┃┃┣╸ ┗━┓ ┃ ┣┳┛┃ ┃",
    "╺┻┛┗━╸┗━┛ ╹ ╹┗╸┗━┛",
];

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
    let w = width.saturating_sub(4).max(10);
    match item {
        Item::User(t) => {
            // a tinted bubble with an accent bar down the left edge
            let bg = theme::sel();
            out.push(Line::raw("").style(bg));
            for l in wrap(t, w) {
                out.push(
                    Line::from(vec![
                        Span::styled(" ▌ ", theme::accent_b()),
                        Span::styled(l, theme::bold()),
                    ])
                    .style(bg),
                );
            }
            out.push(Line::raw("").style(bg));
            out.push(Line::raw(""));
        }
        Item::Assistant(t) => {
            md::layout(t, width, out);
            out.push(Line::raw(""));
        }
        Item::Reasoning(t) => {
            let lines = wrap(t, w.saturating_sub(4));
            let n = lines.len();
            let st = theme::muted().add_modifier(ratatui::style::Modifier::ITALIC);
            for (i, l) in lines.into_iter().take(3).enumerate() {
                let mark = if i == 0 { "◌ " } else { "┊ " };
                out.push(Line::from(vec![
                    Span::styled(format!("  {mark}"), theme::accent2()),
                    Span::styled(l, st),
                ]));
            }
            if n > 3 {
                out.push(Line::styled(
                    format!("  ┊ … {} more lines of reasoning", n - 3),
                    theme::muted(),
                ));
            }
        }
        Item::ToolCall { name, args } => {
            let a = args.replace('\n', " ");
            let room = width.saturating_sub(name.width() + 8);
            out.push(Line::from(vec![
                Span::styled("  ▸ ", theme::accent2()),
                Span::styled(
                    name.clone(),
                    theme::accent2().add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Span::styled(format!("  {}", truncate(&a, room)), theme::muted()),
            ]));
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
                let l = truncate(l, width.saturating_sub(10));
                let (mark, mst) = match (i, *is_error) {
                    (0, true) => ("✗ ", theme::err()),
                    (0, false) => ("✓ ", theme::ok()),
                    _ => ("  ", theme::muted()),
                };
                let st = if *is_error {
                    theme::err()
                } else {
                    theme::muted()
                };
                out.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(mark, mst),
                    Span::styled(l, st),
                ]));
            }
            if lines.len() > 4 {
                out.push(Line::styled(
                    format!("      … {} more lines", lines.len() - 4),
                    theme::faint(),
                ));
            }
            out.push(Line::raw(""));
        }
        Item::Notice(t) => {
            out.push(Line::from(vec![
                Span::styled("  ── ", theme::faint()),
                Span::styled(t.clone(), theme::warn()),
                Span::styled(" ──", theme::faint()),
            ]));
            out.push(Line::raw(""));
        }
    }
}

fn status_glyph(st: Status, tick: u64) -> Span<'static> {
    match st {
        Status::Running => Span::styled(SPIN[(tick as usize) % SPIN.len()], theme::accent_b()),
        Status::Idle => Span::styled("○", theme::muted()),
        Status::Failed => Span::styled("✗", theme::err()),
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

/// Context gauge: green while there's room, amber then red as it fills.
fn gauge(ratio: f64, width: usize) -> Vec<Span<'static>> {
    let t = theme::cur();
    let filled = ((ratio * width as f64).round() as usize).min(width);
    let col = if ratio > 0.85 {
        t.err
    } else if ratio > 0.6 {
        t.warn
    } else {
        t.ok
    };
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(col)),
        Span::styled("░".repeat(width - filled), theme::faint()),
    ]
}

/// `key` label pair: key in accent, label muted.
fn hint(key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(key.to_string(), theme::accent_b()),
        Span::styled(format!(" {label}"), theme::muted()),
    ]
}

/// ` TITLE ───────` section rule.
fn section(title: &str, w: usize) -> Line<'static> {
    let used = title.width() + 2;
    Line::from(vec![
        Span::styled(format!(" {title} "), theme::accent_b()),
        Span::styled("─".repeat(w.saturating_sub(used + 1)), theme::faint()),
    ])
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

/// Rounded popup on the surface colour. Returns the padded inner rect.
fn modal(f: &mut Frame, area: Rect, w: u16, h: u16, title: Line<'static>, border: Style) -> Rect {
    let r = centered(area, w, h);
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title)
        .padding(Padding::new(1, 1, 0, 0))
        .style(theme::surface());
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

fn chip_title(label: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(format!(" {label} "), theme::chip(color)))
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Block::new().style(theme::base()), area);
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
    if app.palette.is_some() {
        draw_palette(f, app, area);
    }
    if app.themes.is_some() {
        draw_theme_picker(f, app, area);
    }
}

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(theme::faint())
        .style(theme::surface());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 6 {
        return;
    }
    let parts = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(inner);
    let w = inner.width as usize;

    // brand
    f.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(" ◆ ", theme::accent_b()),
                Span::styled("DESTRO", theme::bold()),
                Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), theme::muted()),
            ]),
            section("WORKSPACES", w),
        ]),
        parts[0],
    );

    // tree
    let rows = app.rows();
    let cursor = app.cursor_row();
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for (ri, row) in rows.iter().enumerate() {
        let selected = ri == cursor;
        let bg = if selected {
            theme::sel()
        } else {
            Style::default()
        };
        match *row {
            Row::Project(p) => {
                let pr = &app.projects[p];
                let run = pr.running_count();
                let (conn, conn_st) = match pr.conn {
                    Conn::Starting => (" …", theme::muted()),
                    Conn::Ready => ("", theme::muted()),
                    Conn::Dead => (" ✗", theme::err()),
                };
                let caret = if pr.collapsed { "▸" } else { "▾" };
                let (badge, badge_st) = if !pr.approvals.is_empty() {
                    (
                        format!(" ! {} ", pr.approvals.len()),
                        theme::chip(theme::cur().warn),
                    )
                } else if run > 0 {
                    (format!(" ● {run} "), theme::accent_b())
                } else {
                    (String::new(), Style::default())
                };
                let name = truncate(&pr.name, w.saturating_sub(6 + badge.chars().count()));
                lines.push(
                    Line::from(vec![
                        Span::styled(format!(" {caret} "), theme::muted()),
                        Span::styled(
                            name,
                            if selected {
                                theme::accent_b()
                            } else {
                                theme::bold()
                            },
                        ),
                        Span::styled(conn, conn_st),
                        Span::raw(" "),
                        Span::styled(badge, badge_st),
                    ])
                    .style(bg),
                );
            }
            Row::Session(p, s, depth) => {
                let pr = &app.projects[p];
                let se = &pr.sessions[s];
                let pend = pr.approvals.iter().any(|a| a.session_id == se.id);
                let pad = "  ".repeat(depth + 1);
                let mark = if pend {
                    Span::styled(
                        "!",
                        theme::warn().add_modifier(ratatui::style::Modifier::BOLD),
                    )
                } else if se.unread {
                    Span::styled("•", theme::accent_b())
                } else {
                    Span::raw(" ")
                };
                let label = truncate(&se.label(), w.saturating_sub(pad.len() + 5));
                let st = if selected {
                    theme::bold()
                } else if se.status == Status::Running {
                    theme::text()
                } else {
                    theme::muted()
                };
                lines.push(
                    Line::from(vec![
                        Span::raw(" "),
                        mark,
                        Span::raw(pad),
                        status_glyph(se.status, app.tick),
                        Span::styled(format!(" {label}"), st),
                    ])
                    .style(bg),
                );
            }
        }
    }
    let h = parts[1].height as usize;
    let start = if cursor >= h { cursor + 1 - h } else { 0 };
    let shown: Vec<Line> = lines.into_iter().skip(start).take(h).collect();
    f.render_widget(Paragraph::new(shown), parts[1]);

    // footer hints
    let mut a = vec![Span::raw(" ")];
    a.extend(hint("Ctrl+P", "commands"));
    let mut b = vec![Span::raw(" ")];
    b.extend(hint("Ctrl+T", "themes"));
    b.push(Span::raw("  "));
    b.extend(hint("F1", "help"));
    f.render_widget(
        Paragraph::new(vec![Line::raw(""), Line::from(a), Line::from(b)]),
        parts[2],
    );
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let tw = area.width.saturating_sub(7) as usize;
    let input_lines = app.input_lines(tw);
    let ih = (input_lines.len() as u16).clamp(1, 5);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(ih + 2),
        Constraint::Length(1),
    ])
    .split(area);

    // ----- header
    let (cp, cs) = (app.cur_project, app.cur_session);
    let pr = &app.projects[cp];
    let mut head: Vec<Span> = vec![Span::raw(" ")];
    if let Some((advisor, slaves)) = app.advisor_models() {
        head.push(Span::styled(" ADVISOR ", theme::chip(theme::cur().accent2)));
        head.push(Span::styled(
            format!(" {advisor} ▸ {slaves}  "),
            theme::muted(),
        ));
    }
    head.push(Span::styled(pr.name.clone(), theme::accent_b()));
    if let Some(si) = cs {
        let se = &pr.sessions[si];
        head.push(Span::styled("  ›  ", theme::faint()));
        head.push(Span::styled(truncate(&se.label(), 40), theme::text()));
        head.push(Span::styled(
            format!("   turn {} · {}", se.turn, pr.model),
            theme::muted(),
        ));
        if se.status == Status::Running {
            head.push(Span::raw("  "));
            head.push(status_glyph(se.status, app.tick));
            head.push(Span::styled(" working", theme::accent()));
        }
        if se.scroll > 0 {
            head.push(Span::styled(
                format!("   ↑ {} lines up", se.scroll),
                theme::warn(),
            ));
        }
        if se.dropped > 0 {
            head.push(Span::styled(
                format!("   {} older items evicted", se.dropped),
                theme::muted(),
            ));
        }
    } else {
        head.push(Span::styled(format!("   {}", pr.model), theme::muted()));
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

    // ----- transcript
    let body = rows[1];
    let empty = cs
        .map(|si| app.projects[cp].sessions[si].items.is_empty())
        .unwrap_or(true);
    if empty {
        draw_welcome(f, app, body);
    } else if let Some(si) = cs {
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
        let scrolled = se.scroll;
        se.cache = cache;
        let inner = Rect {
            x: body.x + 1,
            y: body.y,
            width: body.width.saturating_sub(1),
            height: body.height,
        };
        f.render_widget(Paragraph::new(slice), inner);
        if scrolled > 0 && body.width > 24 {
            let label = " ↓ End to follow ";
            let lw = label.width() as u16;
            let r = Rect {
                x: body.x + body.width - lw - 1,
                y: body.y + body.height.saturating_sub(1),
                width: lw,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(label, theme::chip(theme::cur().accent))),
                r,
            );
        }
    }

    // ----- input box
    let modal_open = app.pending_modal().is_some()
        || app.help
        || app.snip.is_some()
        || app.setup.is_some()
        || app.palette.is_some()
        || app.themes.is_some();
    let running = cs
        .map(|si| app.projects[cp].sessions[si].status == Status::Running)
        .unwrap_or(false);
    let border = if modal_open {
        theme::faint()
    } else if !app.input.is_empty() {
        theme::accent()
    } else {
        theme::muted()
    };
    let mut title: Vec<Span> = vec![Span::styled(" message ", theme::muted())];
    if running {
        title = vec![
            Span::raw(" "),
            status_glyph(Status::Running, app.tick),
            Span::styled(" working — you can queue another prompt ", theme::muted()),
        ];
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Line::from(title));
    let inner = block.inner(rows[2]);
    f.render_widget(block, rows[2]);
    let shown: Vec<Line> = if app.input.is_empty() {
        vec![Line::from(vec![
            Span::styled(" › ", theme::accent_b()),
            Span::styled("Ask anything…   (type / for commands)", theme::muted()),
        ])]
    } else {
        input_lines
            .iter()
            .take(ih as usize)
            .enumerate()
            .map(|(i, l)| {
                Line::from(vec![
                    Span::styled(if i == 0 { " › " } else { "   " }, theme::accent_b()),
                    Span::styled(l.clone(), theme::text()),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(shown), inner);
    let (cx, cy) = app.cursor_xy(tw);
    if !modal_open {
        f.set_cursor_position((inner.x + 3 + cx as u16, inner.y + (cy as u16).min(ih - 1)));
    }

    // slash-command suggestions float above the input
    let matches = app.slash_matches();
    if !matches.is_empty() && !modal_open {
        draw_slash_popup(f, app, &matches, rows[2], area);
    }

    // ----- footer
    let tag_w = format!("◐ {} ", theme::cur().name).width();
    let mut foot: Vec<Span> = vec![Span::raw(" ")];
    if let Some((m, _)) = &app.flash {
        foot.push(Span::styled("● ", theme::accent()));
        foot.push(Span::styled(m.clone(), theme::bold()));
    } else if app.pending_modal().is_some() {
        foot.extend(hint("y", "allow once"));
        foot.push(Span::raw("   "));
        foot.extend(hint("n", "reject"));
        foot.push(Span::raw("   "));
        foot.extend(hint("Esc", "decide later"));
    } else {
        let mut budget = (area.width as usize).saturating_sub(tag_w + 3);
        let hints: [(&str, &str); 6] = [
            ("Enter", "send"),
            ("Alt+Enter", "newline"),
            ("Alt+↑↓", "switch"),
            ("Ctrl+P", "commands"),
            ("Ctrl+T", "themes"),
            ("F1", "help"),
        ];
        for (kk, l) in hints {
            let need = kk.width() + l.width() + 4;
            if need > budget {
                break;
            }
            budget -= need;
            foot.extend(hint(kk, l));
            foot.push(Span::raw("   "));
        }
    }
    let tag = format!("◐ {} ", theme::cur().name);
    let tagw = tag.width() as u16;
    if area.width > tagw + 30 {
        let r = Rect {
            x: rows[3].x + rows[3].width - tagw,
            width: tagw,
            ..rows[3]
        };
        let left = Rect {
            width: rows[3].width - tagw,
            ..rows[3]
        };
        f.render_widget(Paragraph::new(Line::from(foot)), left);
        f.render_widget(Paragraph::new(Span::styled(tag, theme::muted())), r);
    } else {
        f.render_widget(Paragraph::new(Line::from(foot)), rows[3]);
    }
}

fn draw_slash_popup(f: &mut Frame, app: &App, matches: &[&'static Cmd], input: Rect, area: Rect) {
    let n = matches.len().min(8) as u16;
    let w = 64.min(area.width.saturating_sub(2)).max(24);
    let h = n + 2;
    if input.y < area.y + h {
        return;
    }
    let r = Rect {
        x: input.x + 1,
        y: input.y - h,
        width: w.min(input.width.saturating_sub(2)),
        height: h,
    };
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .style(theme::surface());
    let inner = block.inner(r);
    f.render_widget(block, r);
    let sel = app.slash_sel.min(matches.len() - 1);
    let start = sel.saturating_sub(n as usize - 1);
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(n as usize)
        .map(|(i, c)| cmd_line(c, inner.width as usize, i == sel))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// One command row: name, description, shortcut right-aligned.
fn cmd_line(c: &Cmd, w: usize, selected: bool) -> Line<'static> {
    let bg = if selected {
        theme::sel()
    } else {
        Style::default()
    };
    let name = format!(" {:<10}", c.name);
    let keys = if c.keys.is_empty() {
        String::new()
    } else {
        format!(" {} ", c.keys)
    };
    let room = w.saturating_sub(name.width() + keys.width() + 2);
    let desc = truncate(c.desc, room);
    let gap = w.saturating_sub(name.width() + desc.width() + keys.width());
    Line::from(vec![
        Span::styled(
            name,
            if selected {
                theme::accent_b()
            } else {
                theme::accent()
            },
        ),
        Span::styled(
            desc,
            if selected {
                theme::text()
            } else {
                theme::muted()
            },
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            keys,
            theme::faint().patch(if selected {
                theme::muted()
            } else {
                Style::default()
            }),
        ),
    ])
    .style(bg)
}

fn draw_welcome(f: &mut Frame, app: &App, area: Rect) {
    let pr: &Project = &app.projects[app.cur_project];
    let lw = 56usize;
    let pad = " ".repeat((area.width as usize).saturating_sub(lw) / 2);
    let t = theme::cur();
    let mut l: Vec<Line> = vec![Line::raw("")];
    if area.height >= 20 {
        l.push(Line::raw(""));
    }
    for row in LOGO {
        let n = row.chars().count().max(1);
        let mut spans = vec![Span::raw(pad.clone())];
        for (i, ch) in row.chars().enumerate() {
            let c = theme::lerp(t.accent, t.accent2, i as f32 / (n - 1) as f32);
            spans.push(Span::styled(ch.to_string(), Style::new().fg(c)));
        }
        l.push(Line::from(spans));
    }
    l.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled("the agent harness for your terminal", theme::muted()),
    ]));
    l.push(Line::raw(""));
    let info = |label: &str, value: String| {
        Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(format!("{label:<10}"), theme::muted()),
            Span::styled(truncate(&value, lw.saturating_sub(10)), theme::text()),
        ])
    };
    l.push(info("workspace", pr.name.clone()));
    l.push(info("path", pr.cwd.clone()));
    l.push(info("model", pr.model.clone()));
    l.push(Line::raw(""));
    let mut sec = section("GET STARTED", lw);
    sec.spans.insert(0, Span::raw(pad.clone()));
    l.push(sec);
    let tips: [(&str, &str); 6] = [
        ("Enter", "type a prompt and send it to the agent"),
        ("/", "slash commands — Tab completes"),
        ("Ctrl+P", "command palette: search everything"),
        ("Ctrl+T", "pick a colour theme (live preview)"),
        ("Alt+A", "advisor mode: planner + parallel workers"),
        ("Alt+O", "open another workspace folder"),
    ];
    for (key, desc) in tips {
        l.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(format!(" {key:<8}"), theme::accent_b()),
            Span::styled(desc, theme::text()),
        ]));
    }
    if !pr.log.is_empty() {
        l.push(Line::raw(""));
        for s in pr
            .log
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            l.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(
                    format!(
                        " runtime  {}",
                        truncate(s, area.width.saturating_sub(18) as usize)
                    ),
                    theme::muted(),
                ),
            ]));
        }
    }
    f.render_widget(Paragraph::new(l), area);
}

fn draw_right(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme::faint())
        .style(theme::surface());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let pr = &app.projects[app.cur_project];
    let w = inner.width as usize;
    let mut l: Vec<Line> = vec![Line::raw("")];
    let Some(si) = app.cur_session else {
        l.push(section("RUNTIME", w));
        let (txt, st) = match pr.conn {
            Conn::Starting => ("◌ starting…", theme::muted()),
            Conn::Ready => ("● ready", theme::ok()),
            Conn::Dead => ("✗ not running", theme::err()),
        };
        l.push(Line::styled(format!(" {txt}"), st));
        l.push(Line::raw(""));
        l.extend(balance_lines(&app.balance));
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
        l.push(section("ADVISOR", w));
        l.push(Line::from(vec![
            Span::styled(" plans   ", theme::muted()),
            Span::styled(truncate(advisor, w.saturating_sub(10)), theme::accent_b()),
        ]));
        l.push(Line::from(vec![
            Span::styled(" workers ", theme::muted()),
            Span::styled(truncate(slaves, w.saturating_sub(10)), theme::accent2()),
        ]));
        l.push(Line::styled(
            format!(" {}/{} workers reported", reported, kids.len()),
            theme::muted(),
        ));
        l.push(Line::raw(""));
    }
    l.push(section("AGENTS", w));
    l.push(Line::styled(
        format!(
            " {} running / {} total",
            run + (pr.sessions[root].status == Status::Running) as usize,
            kids.len() + 1
        ),
        theme::muted(),
    ));
    let mut push_agent = |s: &Session, name: &str, sel: bool, tree: &str| {
        let bg = if sel { theme::sel() } else { Style::default() };
        l.push(
            Line::from(vec![
                Span::styled(format!(" {tree}"), theme::faint()),
                status_glyph(s.status, app.tick),
                Span::styled(
                    format!(" {}", truncate(name, w.saturating_sub(7))),
                    if sel { theme::bold() } else { theme::text() },
                ),
            ])
            .style(bg),
        );
        if let Some(t) = &s.last_tool
            && s.status == Status::Running
        {
            l.push(Line::styled(
                format!("     ▸ {}", truncate(t, w.saturating_sub(8))),
                theme::muted(),
            ));
        }
    };
    push_agent(&pr.sessions[root], "main", root == si, "");
    let nk = kids.len();
    for (i, &c) in kids.iter().enumerate() {
        let name = pr.sessions[c].label();
        push_agent(
            &pr.sessions[c],
            &name,
            c == si,
            if i + 1 == nk { "└ " } else { "├ " },
        );
    }
    l.push(Line::raw(""));

    let se = &pr.sessions[si];
    l.push(section("CONTEXT", w));
    match se.ctx_ratio() {
        Some(r) => {
            let mut g = vec![Span::raw(" ")];
            g.extend(gauge(r, w.saturating_sub(8)));
            g.push(Span::styled(
                format!(" {:>3}%", (r * 100.0).round() as u32),
                theme::bold(),
            ));
            l.push(Line::from(g));
            l.push(Line::styled(
                format!(
                    " {} / {} tokens",
                    k(se.usage.last_context),
                    k(se.context_window.unwrap_or(0))
                ),
                theme::muted(),
            ));
        }
        None => l.push(Line::styled(
            format!(" {} tokens", k(se.usage.last_context)),
            theme::muted(),
        )),
    }
    l.push(Line::raw(""));
    let (mut ti, mut to, mut tc) = (0, 0, 0);
    for s in std::iter::once(&pr.sessions[root]).chain(kids.iter().map(|&c| &pr.sessions[c])) {
        ti += s.usage.input;
        to += s.usage.output;
        tc += s.usage.cache_read;
    }
    l.push(section("TOKENS", w));
    let row = |label: &str, v: u64| {
        Line::from(vec![
            Span::styled(format!(" {label:<8}"), theme::muted()),
            Span::styled(format!("{:>8}", k(v)), theme::text()),
        ])
    };
    l.push(row("input", ti));
    l.push(row("output", to));
    l.push(row("cached", tc));
    if let Some(pct) = (tc * 100).checked_div(ti + tc) {
        l.push(Line::from(vec![
            Span::styled(" hit rate", theme::muted()),
            Span::styled(format!("{:>6}%", pct), theme::ok()),
        ]));
    }
    l.push(Line::raw(""));
    l.extend(balance_lines(&app.balance));
    f.render_widget(Paragraph::new(l), inner);
}

fn draw_advisor_setup(f: &mut Frame, app: &App, area: Rect) {
    use crate::app::MODEL_SUGGESTIONS;
    let Some(setup) = &app.setup else {
        return;
    };
    let inner = modal(
        f,
        area,
        76,
        15,
        chip_title("ADVISOR MODE", theme::cur().accent2),
        theme::accent2(),
    );
    let mut lines: Vec<Line> = vec![
        Line::raw(""),
        Line::styled(
            "The advisor plans and judges. Workers execute in parallel.",
            theme::muted(),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  task ", theme::muted()),
            Span::styled("──▶ ", theme::faint()),
            Span::styled("advisor ", theme::accent_b()),
            Span::styled("──▶ ", theme::faint()),
            Span::styled("workers ", theme::accent2()),
            Span::styled("──▶ ", theme::faint()),
            Span::styled("advisor ", theme::accent_b()),
            Span::styled("──▶ ", theme::faint()),
            Span::styled("result", theme::ok()),
        ]),
        Line::raw(""),
    ];
    let labels = ["advisor model", "worker models"];
    for (i, label) in labels.iter().enumerate() {
        let focused = setup.focus == i;
        let value = &setup.models[i];
        let mut spans = vec![
            Span::styled(
                format!(" {label} "),
                if focused {
                    theme::chip(theme::cur().accent)
                } else {
                    theme::muted()
                },
            ),
            Span::raw("  "),
            Span::styled(
                format!(" {value}"),
                if focused {
                    theme::bold()
                } else {
                    theme::text()
                },
            ),
        ];
        if focused {
            spans.push(Span::styled("▏", theme::accent_b()));
        }
        lines.push(Line::from(spans));
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(vec![
        Span::styled(" suggestions  ", theme::muted()),
        Span::styled(MODEL_SUGGESTIONS.join("  ·  "), theme::text()),
    ]));
    lines.push(Line::raw(""));
    let mut h = vec![Span::raw(" ")];
    h.extend(hint("Tab", "switch field"));
    h.push(Span::raw("   "));
    h.extend(hint("Enter", "start"));
    h.push(Span::raw("   "));
    h.extend(hint("Esc", "cancel"));
    lines.push(Line::from(h));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_approval(f: &mut Frame, app: &App, (p, a): (usize, usize), area: Rect) {
    let ap = &app.projects[p].approvals[a];
    let who = app.projects[p]
        .index
        .get(&ap.session_id)
        .map(|&i| app.projects[p].sessions[i].label())
        .unwrap_or_default();
    let inner = modal(
        f,
        area,
        68,
        11,
        chip_title("⚠ APPROVAL NEEDED", theme::cur().warn),
        theme::warn(),
    );
    let mut l = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled(app.projects[p].name.clone(), theme::accent_b()),
            Span::styled("  ›  ", theme::faint()),
            Span::styled(format!("agent {who}"), theme::muted()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("wants to run  ", theme::muted()),
            Span::styled(
                ap.tool.clone(),
                theme::accent2().add_modifier(ratatui::style::Modifier::BOLD),
            ),
        ]),
    ];
    for (i, line) in wrap(
        ap.reason.as_deref().unwrap_or("(no reason given)"),
        (inner.width as usize).saturating_sub(10),
    )
    .into_iter()
    .take(2)
    .enumerate()
    {
        l.push(Line::from(vec![
            Span::styled(
                if i == 0 {
                    "because       "
                } else {
                    "              "
                },
                theme::muted(),
            ),
            Span::styled(line, theme::text()),
        ]));
    }
    l.push(Line::raw(""));
    l.push(Line::from(vec![
        Span::styled(" y  Allow once ", theme::chip(theme::cur().ok)),
        Span::raw("   "),
        Span::styled(" n  Reject ", theme::chip(theme::cur().err)),
        Span::raw("   "),
        Span::styled("Esc  decide later", theme::muted()),
    ]));
    f.render_widget(Paragraph::new(l), inner);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let wide = area.width >= 104;
    let (w, h) = if wide { (100, 22) } else { (66, 40) };
    let inner = modal(
        f,
        area,
        w,
        h,
        chip_title("KEYS", theme::cur().accent),
        theme::accent(),
    );
    let key = |a: &str, b: &str| {
        Line::from(vec![
            Span::styled(format!(" {a:<17}"), theme::accent_b()),
            Span::styled(b.to_string(), theme::text()),
        ])
    };
    let sec = |t: &str, w: usize| section(t, w);
    let half = if wide {
        inner.width as usize / 2 - 1
    } else {
        inner.width as usize
    };
    let mut left: Vec<Line> = vec![
        sec("NAVIGATE", half),
        key("Alt+↑ / Alt+↓", "previous / next workspace or session"),
        key("Alt+1 … Alt+9", "jump to workspace"),
        key("Alt+N", "new session in this workspace"),
        key("PgUp / PgDn", "scroll the transcript"),
        key("↑ / ↓", "scroll too (when the prompt is empty)"),
        key("End", "jump to the newest message"),
        Line::raw(""),
        sec("PROMPT", half),
        key("Enter", "send (slash commands run locally)"),
        key("Alt+Enter", "newline"),
        key("/", "command suggestions · Tab completes"),
        key("Ctrl+U / Ctrl+W", "clear prompt / delete word"),
    ];
    let mut right: Vec<Line> = vec![
        sec("WORKSPACE", half),
        key("Ctrl+P", "command palette — search everything"),
        key("Ctrl+T", "colour themes with live preview"),
        key("Alt+O  /open", "add a workspace (Tab completes dirs)"),
        key("/close", "close the workspace"),
        key("Alt+A  /advisor", "planner + workers · /normal leaves"),
        key("Ctrl+K", "abort this workspace's runtime"),
        Line::raw(""),
        sec("PANELS", half),
        key("Ctrl+S", "snippet viewer (browse, pan, copy)"),
        key("Ctrl+Y", "copy the newest snippet"),
        key("Ctrl+B / Ctrl+G", "toggle sidebar / agents panel"),
        key("Ctrl+A", "reopen a pending approval"),
        key("F1  ·  Ctrl+Q", "this help  ·  quit"),
    ];
    let legend = Line::from(vec![
        Span::styled(" ⠋", theme::accent_b()),
        Span::styled(" running   ", theme::muted()),
        Span::styled("○", theme::muted()),
        Span::styled(" idle   ", theme::muted()),
        Span::styled("✗", theme::err()),
        Span::styled(" failed   ", theme::muted()),
        Span::styled("!", theme::warn()),
        Span::styled(" needs you   ", theme::muted()),
        Span::styled("•", theme::accent_b()),
        Span::styled(" unread", theme::muted()),
    ]);
    if wide {
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        f.render_widget(Paragraph::new(left), cols[0]);
        f.render_widget(Paragraph::new(right), cols[1]);
        f.render_widget(Paragraph::new(vec![Line::raw(""), legend]), rows[1]);
    } else {
        left.push(Line::raw(""));
        left.append(&mut right);
        left.push(Line::raw(""));
        left.push(legend);
        f.render_widget(Paragraph::new(left), inner);
    }
}

fn draw_palette(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = &app.palette else { return };
    let matches = app.palette_matches();
    let n = matches.len().clamp(1, 12) as u16;
    let inner = modal(
        f,
        area,
        70,
        n + 6,
        chip_title("COMMANDS", theme::cur().accent),
        theme::accent(),
    );
    let mut l: Vec<Line> = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled(" › ", theme::accent_b()),
            Span::styled(p.query.clone(), theme::bold()),
            Span::styled("▏", theme::accent_b()),
        ]),
        section("", inner.width as usize),
    ];
    if matches.is_empty() {
        l.push(Line::styled(" no command matches", theme::muted()));
    }
    let sel = p.sel.min(matches.len().saturating_sub(1));
    let start = sel.saturating_sub(n as usize - 1);
    for (i, c) in matches.iter().enumerate().skip(start).take(n as usize) {
        l.push(cmd_line(c, inner.width as usize, i == sel));
    }
    l.push(Line::raw(""));
    let mut h = vec![Span::raw(" ")];
    h.extend(hint("↑↓", "select"));
    h.push(Span::raw("   "));
    h.extend(hint("Enter", "run"));
    h.push(Span::raw("   "));
    h.extend(hint("Esc", "close"));
    l.push(Line::from(h));
    f.render_widget(Paragraph::new(l), inner);
}

fn draw_theme_picker(f: &mut Frame, app: &App, area: Rect) {
    let Some(tp) = &app.themes else { return };
    let inner = modal(
        f,
        area,
        50,
        THEMES.len() as u16 + 6,
        chip_title("THEMES", theme::cur().accent),
        theme::accent(),
    );
    let mut l: Vec<Line> = vec![Line::raw("")];
    for (i, t) in THEMES.iter().enumerate() {
        let sel = i == tp.sel;
        let bg = if sel { theme::sel() } else { Style::default() };
        let sw = |c: Color| Span::styled("██", Style::new().fg(c));
        l.push(
            Line::from(vec![
                Span::styled(if sel { " ▸ " } else { "   " }, theme::accent_b()),
                Span::styled(
                    format!("{:<13}", t.name),
                    if sel { theme::bold() } else { theme::text() },
                ),
                sw(t.accent),
                sw(t.accent2),
                sw(t.ok),
                sw(t.warn),
                sw(t.err),
                Span::styled(
                    if i == tp.original {
                        "  ✓ current"
                    } else {
                        ""
                    },
                    theme::muted(),
                ),
            ])
            .style(bg),
        );
    }
    l.push(Line::raw(""));
    let mut h = vec![Span::raw(" ")];
    h.extend(hint("↑↓", "preview"));
    h.push(Span::raw("   "));
    h.extend(hint("Enter", "apply & save"));
    h.push(Span::raw("   "));
    h.extend(hint("Esc", "cancel"));
    l.push(Line::from(h));
    f.render_widget(Paragraph::new(l), inner);
}

fn balance_header(b: &Balance) -> Option<Span<'static>> {
    match b {
        Balance::Unsupported => None,
        Balance::NoKey => Some(Span::styled(" balance: no API key ", theme::muted())),
        Balance::Loading => Some(Span::styled(" balance … ", theme::muted())),
        _ => match b.short() {
            Some(s) if b.is_low() => Some(Span::styled(
                format!(" {s} LOW "),
                theme::chip(theme::cur().err),
            )),
            Some(s) => Some(Span::styled(format!(" {s} "), theme::ok())),
            None => Some(Span::styled(" balance ✗ ", theme::muted())),
        },
    }
}

fn balance_lines(b: &Balance) -> Vec<Line<'static>> {
    let mut l = vec![section("BALANCE", 32)];
    let (info, at, err) = match b {
        Balance::Unsupported => {
            l.push(Line::styled(" n/a for this provider", theme::muted()));
            return l;
        }
        Balance::NoKey => {
            l.push(Line::styled(" set DEEPSEEK_API_KEY", theme::muted()));
            l.push(Line::styled(" (env or .env) to show it", theme::muted()));
            return l;
        }
        Balance::Loading => {
            l.push(Line::styled(" loading…", theme::muted()));
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
                Span::styled(format!(" {} ", w.currency), theme::muted()),
                Span::styled(
                    format!("{sym}{}", w.total),
                    if b.is_low() {
                        theme::err().add_modifier(ratatui::style::Modifier::BOLD)
                    } else {
                        theme::ok().add_modifier(ratatui::style::Modifier::BOLD)
                    },
                ),
            ]));
            l.push(Line::styled(
                format!("   topped up {sym}{}", w.topped_up),
                theme::muted(),
            ));
            l.push(Line::styled(
                format!("   granted   {sym}{}", w.granted),
                theme::muted(),
            ));
        }
        if !info.available {
            l.push(Line::styled(" account not available", theme::err()));
        }
    }
    if let Some(at) = at {
        l.push(Line::styled(
            format!(" updated {}s ago", at.elapsed().as_secs()),
            theme::faint(),
        ));
    }
    if let Some(e) = err {
        l.push(Line::styled(
            format!(" ✗ {}", truncate(&e, 28)),
            theme::err(),
        ));
    }
    l
}

fn draw_snippets(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Clear, area);
    let n = app.cur_snippets().len();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(chip_title(&format!("SNIPPETS · {n}"), theme::cur().accent))
        .style(theme::surface());
    let inner = block.inner(area);
    f.render_widget(block, area);
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
        let label = truncate(&s.title, 18);
        let meta = format!(
            "{} · {}L",
            if s.lang.is_empty() { "text" } else { &s.lang },
            s.code.split('\n').count()
        );
        let selected = i == sel;
        list.push(
            Line::from(vec![
                Span::styled(if selected { " ▸ " } else { "   " }, theme::accent_b()),
                Span::styled(
                    format!("{label:<18}"),
                    if selected {
                        theme::bold()
                    } else {
                        theme::text()
                    },
                ),
                Span::styled(format!(" {}", truncate(&meta, 11)), theme::muted()),
            ])
            .style(if selected {
                theme::sel()
            } else {
                Style::default()
            }),
        );
    }
    f.render_widget(
        Paragraph::new(list).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme::faint()),
        ),
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
            Span::styled(format!(" {}", s.title), theme::accent_b()),
            Span::styled(
                format!(
                    "   {} · {} lines · from {}",
                    if s.lang.is_empty() { "text" } else { &s.lang },
                    total_lines,
                    truncate(&who, 40)
                ),
                theme::muted(),
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
            Span::styled(format!(" {:>digits$} ", i + 1), theme::muted()),
            Span::styled("│ ", theme::faint()),
        ];
        row.extend(highlight::to_spans(&toks, h, body_w));
        out.push(Line::from(row));
    }
    f.render_widget(Paragraph::new(out), rows[0]);
    let mut hl_line = vec![Span::raw(" ")];
    hl_line.extend(hint("↑↓", "select"));
    hl_line.push(Span::raw("   "));
    hl_line.extend(hint("PgUp/PgDn", "scroll"));
    hl_line.push(Span::raw("   "));
    hl_line.extend(hint("←→", "pan"));
    hl_line.push(Span::raw("   "));
    hl_line.extend(hint("c", "copy"));
    hl_line.push(Span::raw("   "));
    hl_line.extend(hint("Esc", "close"));
    f.render_widget(
        Paragraph::new(Line::from(hl_line)).alignment(Alignment::Left),
        rows[1],
    );
}
