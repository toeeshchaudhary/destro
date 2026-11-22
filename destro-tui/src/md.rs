//! Markdown-lite layout for assistant text: fenced code blocks (highlighted, with a
//! line-number gutter), headings, bullets, quotes, `inline code` and **bold**.
//! Output is pre-wrapped `Line`s so the transcript cache can slice them cheaply.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::highlight::{self, Highlighter, expand_tabs};

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Every fenced code block in `text` as `(language tag, code)`.
pub fn fences(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("```") {
            match cur.take() {
                Some((tag, body)) => out.push((tag, body.join("\n"))),
                None => cur = Some((rest.trim().to_string(), vec![])),
            }
        } else if let Some((_, body)) = cur.as_mut() {
            body.push(line);
        }
    }
    if let Some((tag, body)) = cur {
        out.push((tag, body.join("\n"))); // unterminated fence (message cut off)
    }
    out
}

type Seg = (String, Style);

/// Split `**bold**` and `` `code` `` out of one line of prose.
fn inline(text: &str, base: Style) -> Vec<Seg> {
    let ch: Vec<char> = text.chars().collect();
    let mut segs: Vec<Seg> = vec![];
    let mut buf = String::new();
    let flush = |buf: &mut String, segs: &mut Vec<Seg>| {
        if !buf.is_empty() {
            segs.push((std::mem::take(buf), base));
        }
    };
    let mut i = 0;
    while i < ch.len() {
        if ch[i] == '`' {
            if let Some(end) = ch[i + 1..].iter().position(|&c| c == '`') {
                flush(&mut buf, &mut segs);
                let code: String = ch[i + 1..i + 1 + end].iter().collect();
                segs.push((code, base.fg(Color::Yellow).bg(Color::Indexed(236))));
                i += end + 2;
                continue;
            }
        }
        if ch[i] == '*' && i + 1 < ch.len() && ch[i + 1] == '*' {
            if let Some(end) =
                (i + 2..ch.len().saturating_sub(1)).find(|&j| ch[j] == '*' && ch[j + 1] == '*')
            {
                flush(&mut buf, &mut segs);
                let b: String = ch[i + 2..end].iter().collect();
                segs.push((b, base.add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }
        buf.push(ch[i]);
        i += 1;
    }
    flush(&mut buf, &mut segs);
    segs
}

/// Greedy word wrap over styled segments, with a first-line and a hanging prefix.
fn wrap_segs(
    segs: Vec<Seg>,
    width: usize,
    first: &str,
    rest: &str,
    prefix_style: Style,
    out: &mut Vec<Line<'static>>,
) {
    let width = width.max(12);
    let mut words: Vec<(String, Style, bool)> = vec![]; // text, style, preceded-by-space
    let mut pending = false;
    for (text, st) in segs {
        for (k, w) in text.split(' ').enumerate() {
            if k > 0 {
                pending = true;
            }
            if !w.is_empty() {
                words.push((w.to_string(), st, pending));
                pending = false;
            }
        }
    }
    let mut lines: Vec<Vec<Span<'static>>> = vec![];
    let mut cur: Vec<Span<'static>> = vec![];
    let mut used = 0usize;
    let pw = first.width();
    let avail = width.saturating_sub(pw);
    for (w, st, sp) in words {
        let ww = w.width();
        let need = ww + if used > 0 && sp { 1 } else { 0 };
        if used > 0 && used + need > avail {
            lines.push(std::mem::take(&mut cur));
            used = 0;
        }
        if ww > avail {
            // hard split
            let mut piece = String::new();
            let mut pu = 0;
            for c in w.chars() {
                let cw = c.width().unwrap_or(0);
                if used + pu + cw > avail {
                    cur.push(Span::styled(std::mem::take(&mut piece), st));
                    lines.push(std::mem::take(&mut cur));
                    used = 0;
                    pu = 0;
                }
                piece.push(c);
                pu += cw;
            }
            cur.push(Span::styled(piece, st));
            used += pu;
            continue;
        }
        if used > 0 && sp {
            cur.push(Span::raw(" "));
            used += 1;
        }
        cur.push(Span::styled(w, st));
        used += ww;
    }
    lines.push(cur);
    for (i, spans) in lines.into_iter().enumerate() {
        let mut l = vec![Span::styled(
            if i == 0 {
                first.to_string()
            } else {
                rest.to_string()
            },
            prefix_style,
        )];
        l.extend(spans);
        out.push(Line::from(l));
    }
}

fn clip(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = vec![];
    let mut used = 0;
    for s in spans {
        let w = s.content.width();
        if used + w <= max {
            used += w;
            out.push(s);
        } else {
            let mut t = String::new();
            for c in s.content.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw >= max {
                    break;
                }
                t.push(c);
                used += cw;
            }
            t.push('…');
            out.push(Span::styled(t, s.style));
            break;
        }
    }
    out
}

/// A highlighted code block with a line-number gutter. `max_lines` bounds the preview.
pub fn code_block(
    tag: &str,
    code: &str,
    width: usize,
    indent: &str,
    max_lines: usize,
    out: &mut Vec<Line<'static>>,
) {
    let lines: Vec<&str> = code.split('\n').collect();
    let total = lines.len();
    let digits = total.to_string().len().max(2);
    let label = if tag.is_empty() { "text" } else { tag };
    out.push(Line::from(vec![
        Span::styled(format!("{indent}╭─ "), dim()),
        Span::styled(
            label.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {total} line{}", if total == 1 { "" } else { "s" }),
            dim(),
        ),
    ]));
    let mut hl = Highlighter::new(tag);
    let body_w = width.saturating_sub(indent.width() + digits + 4);
    for (n, raw) in lines.iter().enumerate() {
        let toks = hl.line(&expand_tabs(raw));
        if n >= max_lines {
            continue;
        }
        let mut row = vec![
            Span::styled(format!("{indent}│ "), dim()),
            Span::styled(format!("{:>digits$} ", n + 1), dim()),
        ];
        row.extend(clip(highlight::to_spans(&toks, 0, usize::MAX), body_w));
        out.push(Line::from(row));
    }
    if total > max_lines {
        out.push(Line::styled(
            format!(
                "{indent}╰─ … {} more lines · Ctrl+S to open",
                total - max_lines
            ),
            dim(),
        ));
    } else {
        out.push(Line::styled(format!("{indent}╰─"), dim()));
    }
}

pub fn layout(text: &str, width: usize, out: &mut Vec<Line<'static>>) {
    let mut fence: Option<(String, Vec<String>)> = None;
    for line in text.split('\n') {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("```") {
            match fence.take() {
                Some((tag, body)) => code_block(&tag, &body.join("\n"), width, "  ", 40, out),
                None => fence = Some((rest.trim().to_string(), vec![])),
            }
            continue;
        }
        if let Some((_, body)) = fence.as_mut() {
            body.push(line.to_string());
            continue;
        }
        if t.is_empty() {
            out.push(Line::raw(""));
        } else if let Some(h) = t
            .strip_prefix("### ")
            .or_else(|| t.strip_prefix("## "))
            .or_else(|| t.strip_prefix("# "))
        {
            let st = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            wrap_segs(inline(h, st), width, "  ", "  ", Style::default(), out);
        } else if let Some(b) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("• "))
        {
            let lead = line.len() - t.len();
            let pad = " ".repeat(lead);
            wrap_segs(
                inline(b, Style::default()),
                width,
                &format!("  {pad}• "),
                &format!("  {pad}  "),
                dim(),
                out,
            );
        } else if let Some(q) = t.strip_prefix("> ") {
            wrap_segs(inline(q, dim()), width, "  ▎ ", "  ▎ ", dim(), out);
        } else if let Some((num, body)) = numbered(t) {
            let lead = line.len() - t.len();
            let pad = " ".repeat(lead);
            let marker = format!("  {pad}{num}. ");
            let hang = " ".repeat(marker.width());
            wrap_segs(
                inline(body, Style::default()),
                width,
                &marker,
                &hang,
                dim(),
                out,
            );
        } else {
            wrap_segs(
                inline(t, Style::default()),
                width,
                "  ",
                "  ",
                Style::default(),
                out,
            );
        }
    }
    if let Some((tag, body)) = fence {
        code_block(&tag, &body.join("\n"), width, "  ", 40, out);
    }
}

fn numbered(t: &str) -> Option<(&str, &str)> {
    let dot = t.find(". ")?;
    let (n, rest) = t.split_at(dot);
    if !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit()) {
        Some((n, &rest[2..]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fences_including_unterminated() {
        let f = fences("hi\n```rust\nfn a() {}\n```\ntext\n```py\nx = 1");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0], ("rust".into(), "fn a() {}".into()));
        assert_eq!(f[1].0, "py");
    }

    #[test]
    fn layout_wraps_within_width() {
        let mut out = vec![];
        layout(
            "a very long line of prose that must wrap onto several rows when narrow",
            30,
            &mut out,
        );
        assert!(out.len() > 2);
        for l in &out {
            let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 30, "row too wide: {w}");
        }
    }
}
