//! Markdown-lite layout for assistant text: fenced code blocks (highlighted, with a
//! line-number gutter), headings, bullets, quotes, `inline code` and **bold**.
//! Output is pre-wrapped `Line`s so the transcript cache can slice them cheaply.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::highlight::{self, Highlighter, expand_tabs};
use crate::theme;

fn dim() -> Style {
    theme::muted()
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

/// Inline markdown to styled segments: `code`, **bold**, __bold__, *italic*,
/// _italic_, ~~strike~~, [links](url), ![images](url), <autolinks>, bare URLs and
/// backslash escapes. Emphasis nests (bold containing italic, code in a link…).
fn inline(text: &str, base: Style) -> Vec<Seg> {
    let ch: Vec<char> = text.chars().collect();
    let mut out = vec![];
    parse_inline(&ch, base, &mut out);
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

/// Index of the first `needle` at or after `from`, honouring backslash escapes.
fn find_seq(ch: &[char], from: usize, needle: &[char]) -> Option<usize> {
    let mut i = from;
    while i + needle.len() <= ch.len() {
        if ch[i] == '\\' {
            i += 2;
            continue;
        }
        if ch[i..i + needle.len()] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn url_end(ch: &[char], start: usize) -> usize {
    let mut j = start;
    while j < ch.len() && !ch[j].is_whitespace() && ch[j] != '<' && ch[j] != '>' {
        j += 1;
    }
    while j > start
        && matches!(
            ch[j - 1],
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
        )
    {
        j -= 1;
    }
    j
}

fn short_url(u: &str) -> String {
    const MAX: usize = 48;
    if u.chars().count() <= MAX {
        u.to_string()
    } else {
        let mut t: String = u.chars().take(MAX - 1).collect();
        t.push('…');
        t
    }
}

fn parse_inline(ch: &[char], base: Style, out: &mut Vec<Seg>) {
    let th = theme::cur();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Seg>| {
        if !buf.is_empty() {
            out.push((std::mem::take(buf), base));
        }
    };
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        // escapes: \* \_ \` \[ …
        if c == '\\' && i + 1 < ch.len() && ch[i + 1].is_ascii_punctuation() {
            buf.push(ch[i + 1]);
            i += 2;
            continue;
        }
        // code span: a run of N backticks closes at the next run of exactly N
        if c == '`' {
            let n = ch[i..].iter().take_while(|&&x| x == '`').count();
            let fence = vec!['`'; n];
            let mut at = i + n;
            let mut close = None;
            while let Some(p) = find_seq(ch, at, &fence) {
                let run = ch[p..].iter().take_while(|&&x| x == '`').count();
                if run == n {
                    close = Some(p);
                    break;
                }
                at = p + run;
            }
            if let Some(p) = close {
                flush(&mut buf, out);
                let code: String = ch[i + n..p].iter().collect();
                out.push((format!(" {} ", code.trim()), base.fg(th.warn).bg(th.code)));
                i = p + n;
                continue;
            }
            buf.extend(std::iter::repeat_n('`', n));
            i += n;
            continue;
        }
        // image / link
        let is_image = c == '!' && ch.get(i + 1) == Some(&'[');
        if c == '[' || is_image {
            let open = if is_image { i + 1 } else { i };
            if let Some(close) = find_seq(ch, open + 1, &[']'])
                && ch.get(close + 1) == Some(&'(')
                && let Some(end) = find_seq(ch, close + 2, &[')'])
            {
                let label: Vec<char> = ch[open + 1..close].to_vec();
                let url: String = ch[close + 2..end].iter().collect();
                let url = url.split_whitespace().next().unwrap_or("").to_string();
                flush(&mut buf, out);
                if is_image {
                    let alt: String = label.iter().collect();
                    out.push((
                        format!("[image: {}]", if alt.is_empty() { &url } else { &alt }),
                        theme::accent2(),
                    ));
                } else {
                    let link = theme::accent().add_modifier(Modifier::UNDERLINED);
                    let before = out.len();
                    parse_inline(&label, link, out);
                    let shown: String = out[before..].iter().map(|(t, _)| t.as_str()).collect();
                    if !url.is_empty() && !url.starts_with('#') && shown.trim() != url {
                        out.push((format!(" ({})", short_url(&url)), dim()));
                    }
                }
                i = end + 1;
                continue;
            }
        }
        // <https://autolink>
        if c == '<' {
            let rest: String = ch[i + 1..].iter().take(8).collect();
            if (rest.starts_with("http://") || rest.starts_with("https://"))
                && let Some(end) = find_seq(ch, i + 1, &['>'])
            {
                flush(&mut buf, out);
                let url: String = ch[i + 1..end].iter().collect();
                out.push((url, theme::accent().add_modifier(Modifier::UNDERLINED)));
                i = end + 1;
                continue;
            }
        }
        // bare URL at a word boundary
        if c == 'h' && (i == 0 || !is_word(ch[i - 1])) {
            let rest: String = ch[i..].iter().take(8).collect();
            if rest.starts_with("http://") || rest.starts_with("https://") {
                let end = url_end(ch, i);
                if end > i + 8 {
                    flush(&mut buf, out);
                    let url: String = ch[i..end].iter().collect();
                    out.push((url, theme::accent().add_modifier(Modifier::UNDERLINED)));
                    i = end;
                    continue;
                }
            }
        }
        // strikethrough
        if c == '~'
            && ch.get(i + 1) == Some(&'~')
            && let Some(end) = find_seq(ch, i + 2, &['~', '~'])
            && end > i + 2
        {
            flush(&mut buf, out);
            parse_inline(
                &ch[i + 2..end],
                base.add_modifier(Modifier::CROSSED_OUT),
                out,
            );
            i = end + 2;
            continue;
        }
        // strong: ** or __
        if (c == '*' || c == '_') && ch.get(i + 1) == Some(&c) {
            let boundary_ok = c == '*' || i == 0 || !is_word(ch[i - 1]);
            if boundary_ok
                && ch.get(i + 2).is_some_and(|x| !x.is_whitespace())
                && let Some(end) = find_seq(ch, i + 2, &[c, c])
                && end > i + 2
                && !ch[end - 1].is_whitespace()
                && (c == '*' || ch.get(end + 2).is_none_or(|x| !is_word(*x)))
            {
                flush(&mut buf, out);
                parse_inline(&ch[i + 2..end], base.add_modifier(Modifier::BOLD), out);
                i = end + 2;
                continue;
            }
        }
        // emphasis: * or _ (underscores only at word boundaries: snake_case stays)
        if c == '*' || c == '_' {
            let boundary_ok = c == '*' || i == 0 || !is_word(ch[i - 1]);
            if boundary_ok && ch.get(i + 1).is_some_and(|x| !x.is_whitespace() && *x != c) {
                let mut at = i + 1;
                let mut close = None;
                while let Some(p) = find_seq(ch, at, &[c]) {
                    let prev_ok = p > i + 1 && !ch[p - 1].is_whitespace();
                    let doubled =
                        ch.get(p + 1) == Some(&c) || ch.get(p.wrapping_sub(1)) == Some(&c);
                    let next_ok = c == '*' || ch.get(p + 1).is_none_or(|x| !is_word(*x));
                    if prev_ok && !doubled && next_ok {
                        close = Some(p);
                        break;
                    }
                    at = p + 1;
                }
                if let Some(p) = close {
                    flush(&mut buf, out);
                    parse_inline(&ch[i + 1..p], base.add_modifier(Modifier::ITALIC), out);
                    i = p + 1;
                    continue;
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, out);
}

/// Greedy word wrap of styled segments into lines of at most `avail` columns.
fn wrap_spans(segs: Vec<Seg>, avail: usize) -> Vec<Vec<Span<'static>>> {
    let avail = avail.max(1);
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
    for (w, st, sp) in words {
        let ww = w.width();
        let need = ww + if used > 0 && sp { 1 } else { 0 };
        if used > 0 && used + need > avail {
            lines.push(std::mem::take(&mut cur));
            used = 0;
        }
        if ww > avail {
            // hard split a token longer than the line
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
    lines
}

/// Wrap with a first-line and a hanging prefix.
fn wrap_segs(
    segs: Vec<Seg>,
    width: usize,
    first: &str,
    rest: &str,
    prefix_style: Style,
    out: &mut Vec<Line<'static>>,
) {
    let width = width.max(12);
    let avail = width.saturating_sub(first.width());
    for (i, spans) in wrap_spans(segs, avail).into_iter().enumerate() {
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

/// A highlighted code block with a line-number gutter on a tinted background.
/// `max_lines` bounds the preview.
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
    let bg = theme::code_bg();
    let edge = theme::faint().patch(bg);
    out.push(
        Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(" ", bg),
            Span::styled(format!(" {label} "), theme::chip(theme::cur().accent2)),
            Span::styled(
                format!("  {total} line{}", if total == 1 { "" } else { "s" }),
                dim().patch(bg),
            ),
        ])
        .style(bg),
    );
    let mut hl = Highlighter::new(tag);
    let body_w = width.saturating_sub(indent.width() + digits + 4);
    for (n, raw) in lines.iter().enumerate() {
        let toks = hl.line(&expand_tabs(raw));
        if n >= max_lines {
            continue;
        }
        let mut row = vec![
            Span::raw(indent.to_string()),
            Span::styled(format!(" {:>digits$} ", n + 1), dim().patch(bg)),
            Span::styled("│ ", edge),
        ];
        row.extend(clip(highlight::to_spans(&toks, 0, usize::MAX), body_w));
        out.push(Line::from(row).style(bg));
    }
    let foot = if total > max_lines {
        format!(" … {} more lines · Ctrl+S to open", total - max_lines)
    } else {
        " ".to_string()
    };
    out.push(
        Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(foot, dim().patch(bg)),
        ])
        .style(bg),
    );
}

fn seg_text(segs: &[Seg]) -> String {
    segs.iter().map(|(t, _)| t.as_str()).collect()
}

fn is_rule(t: &str) -> bool {
    let s: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    s.len() >= 3
        && ["-", "*", "_"]
            .iter()
            .any(|m| s.chars().all(|c| c.to_string() == *m))
}

/// `# title` → (level, text without the marker or trailing #s).
fn heading(t: &str) -> Option<(usize, &str)> {
    let level = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = t[level..].strip_prefix(' ')?;
    Some((level, rest.trim().trim_end_matches('#').trim_end()))
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = vec![];
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// The alignment row of a pipe table (`|:--|:-:|--:|`), if `line` is one.
fn table_alignments(line: &str) -> Option<Vec<Align>> {
    // a lone `---` is a rule, not a table
    if !line.contains('-') || !line.contains('|') {
        return None;
    }
    let cells = split_row(line);
    let mut aligns = vec![];
    for c in &cells {
        let b = c.trim();
        if b.is_empty() || !b.chars().all(|ch| ch == '-' || ch == ':') || !b.contains('-') {
            return None;
        }
        aligns.push(match (b.starts_with(':'), b.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    (!aligns.is_empty()).then_some(aligns)
}

/// A GitHub-style table drawn with box characters; cells wrap to fit `width`.
fn table(
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    aligns: &[Align],
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let ncols = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let cell = |r: &Vec<String>, c: usize| r.get(c).cloned().unwrap_or_default();
    let mut grid: Vec<Vec<Vec<Seg>>> = vec![];
    let head_style = theme::accent_b();
    grid.push(
        (0..ncols)
            .map(|c| inline(&cell(&header, c), head_style))
            .collect(),
    );
    for r in &rows {
        grid.push(
            (0..ncols)
                .map(|c| inline(&cell(r, c), theme::text()))
                .collect(),
        );
    }
    let natural: Vec<usize> = (0..ncols)
        .map(|c| {
            grid.iter()
                .map(|row| seg_text(&row[c]).width())
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    // borders: "│ " + cells separated by " │ " + " │" = 3*ncols + 1, plus the indent
    let overhead = 2 + 3 * ncols + 1;
    let avail = width.saturating_sub(overhead);
    let mut w = natural.clone();
    if avail < ncols * 3 {
        // too narrow for a grid: fall back to "a | b | c" lines
        for (i, row) in grid.iter().enumerate() {
            let segs: Vec<Seg> = row
                .iter()
                .enumerate()
                .flat_map(|(c, cell)| {
                    let mut v = cell.clone();
                    if c + 1 < row.len() {
                        v.push((" | ".to_string(), theme::faint()));
                    }
                    v
                })
                .collect();
            wrap_segs(segs, width, "  ", "  ", theme::text(), out);
            if i == 0 {
                out.push(Line::styled(
                    format!("  {}", "─".repeat(width.saturating_sub(4).min(40))),
                    theme::faint(),
                ));
            }
        }
        return;
    }
    while w.iter().sum::<usize>() > avail {
        // shave the widest column
        let (i, _) = w.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
        if w[i] <= 3 {
            break;
        }
        w[i] -= 1;
    }
    let edge = theme::faint();
    let rule = |l: &str, m: &str, r: &str| {
        let mut s = format!("  {l}");
        for (i, cw) in w.iter().enumerate() {
            s.push_str(&"─".repeat(cw + 2));
            s.push_str(if i + 1 < w.len() { m } else { r });
        }
        Line::styled(s, edge)
    };
    out.push(rule("┌", "┬", "┐"));
    for (ri, row) in grid.iter().enumerate() {
        let cells: Vec<Vec<Vec<Span<'static>>>> = row
            .iter()
            .enumerate()
            .map(|(c, segs)| wrap_spans(segs.clone(), w[c]))
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for line in 0..height {
            let mut spans = vec![Span::raw("  "), Span::styled("│", edge)];
            for (c, lines) in cells.iter().enumerate() {
                let content = lines.get(line).cloned().unwrap_or_default();
                let used: usize = content.iter().map(|s| s.content.width()).sum();
                let gap = w[c].saturating_sub(used);
                let (l, r) = match aligns.get(c).copied().unwrap_or(Align::Left) {
                    Align::Left => (0, gap),
                    Align::Right => (gap, 0),
                    Align::Center => (gap / 2, gap - gap / 2),
                };
                spans.push(Span::raw(format!(" {}", " ".repeat(l))));
                spans.extend(content);
                spans.push(Span::raw(format!("{} ", " ".repeat(r))));
                spans.push(Span::styled("│", edge));
            }
            out.push(Line::from(spans));
        }
        if ri == 0 {
            out.push(rule("├", "┼", "┤"));
        }
    }
    out.push(rule("└", "┴", "┘"));
}

fn bullet_glyph(depth: usize) -> &'static str {
    match depth {
        0 => "•",
        1 => "◦",
        _ => "▪",
    }
}

pub fn layout(text: &str, width: usize, out: &mut Vec<Line<'static>>) {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut fence: Option<(String, Vec<String>)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
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
            continue;
        }
        // pipe table: a header row followed by an alignment row
        if t.contains('|')
            && let Some(sep) = lines.get(i)
            && let Some(aligns) = table_alignments(sep)
        {
            let header = split_row(t);
            if header.len() == aligns.len() || header.len() > 1 {
                let mut rows = vec![];
                let mut j = i + 1;
                while j < lines.len() && lines[j].contains('|') && !lines[j].trim().is_empty() {
                    rows.push(split_row(lines[j]));
                    j += 1;
                }
                table(header, rows, &aligns, width, out);
                i = j;
                continue;
            }
        }
        if is_rule(t) {
            out.push(Line::styled(
                format!("  {}", "─".repeat(width.saturating_sub(4).max(3))),
                theme::faint(),
            ));
        } else if let Some((level, h)) = heading(t) {
            out.push(Line::raw(""));
            let st = if level >= 3 {
                theme::accent2().add_modifier(Modifier::BOLD)
            } else {
                theme::accent_b()
            };
            wrap_segs(inline(h, st), width, "  ", "  ", theme::text(), out);
            if level == 1 {
                out.push(Line::styled(
                    format!("  {}", "━".repeat(width.saturating_sub(4).max(3))),
                    theme::faint(),
                ));
            }
        } else if let Some(b) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
            .or_else(|| t.strip_prefix("• "))
        {
            let lead = line.len() - t.len();
            let depth = lead / 2;
            let pad = " ".repeat(lead);
            // task list items
            let (marker, mstyle, body) = if let Some(x) = b.strip_prefix("[ ] ") {
                ("☐", theme::muted(), x)
            } else if let Some(x) = b.strip_prefix("[x] ").or_else(|| b.strip_prefix("[X] ")) {
                ("☑", theme::ok(), x)
            } else {
                (bullet_glyph(depth), theme::accent(), b)
            };
            wrap_segs(
                inline(body, theme::text()),
                width,
                &format!("  {pad}{marker} "),
                &format!("  {pad}  "),
                mstyle,
                out,
            );
        } else if t.starts_with('>') {
            let depth = t
                .chars()
                .take_while(|&c| c == '>' || c == ' ')
                .filter(|&c| c == '>')
                .count();
            let q = t.trim_start_matches(['>', ' ']);
            let bar = format!("  {} ", "▎".repeat(depth));
            wrap_segs(
                inline(q, dim().add_modifier(Modifier::ITALIC)),
                width,
                &bar,
                &bar,
                theme::accent2(),
                out,
            );
        } else if let Some((num, body)) = numbered(t) {
            let lead = line.len() - t.len();
            let pad = " ".repeat(lead);
            let marker = format!("  {pad}{num}. ");
            let hang = " ".repeat(marker.width());
            wrap_segs(
                inline(body, theme::text()),
                width,
                &marker,
                &hang,
                theme::accent(),
                out,
            );
        } else {
            wrap_segs(
                inline(t, theme::text()),
                width,
                "  ",
                "  ",
                theme::text(),
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

    fn plain(segs: &[Seg]) -> String {
        segs.iter().map(|(t, _)| t.as_str()).collect()
    }

    fn render(text: &str, width: usize) -> Vec<String> {
        let mut out = vec![];
        layout(text, width, &mut out);
        out.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn inline_styles_nest_and_keep_text() {
        let segs = inline("a **bold *both* more** _it_ ~~gone~~ `x`", Style::default());
        let text = plain(&segs);
        assert_eq!(text, "a bold both more it gone  x ");
        let find = |s: &str| segs.iter().find(|(t, _)| t == s).unwrap().1;
        assert!(find("bold ").add_modifier.contains(Modifier::BOLD));
        let both = find("both");
        assert!(
            both.add_modifier.contains(Modifier::BOLD)
                && both.add_modifier.contains(Modifier::ITALIC)
        );
        assert!(find("it").add_modifier.contains(Modifier::ITALIC));
        assert!(find("gone").add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn snake_case_math_and_escapes_are_left_alone() {
        assert_eq!(
            plain(&inline(
                "call my_func_name and snake_case_var",
                Style::default()
            )),
            "call my_func_name and snake_case_var"
        );
        assert_eq!(plain(&inline("2 * 3 * 4", Style::default())), "2 * 3 * 4");
        assert_eq!(
            plain(&inline(r"\*not italic\* and \_x\_", Style::default())),
            "*not italic* and _x_"
        );
        assert_eq!(
            plain(&inline("unclosed **bold and `tick", Style::default())),
            "unclosed **bold and `tick"
        );
    }

    #[test]
    fn links_images_and_urls() {
        let t = plain(&inline(
            "see [the docs](https://example.com/docs) now",
            Style::default(),
        ));
        assert_eq!(t, "see the docs (https://example.com/docs) now");
        let anchor = plain(&inline("[top](#top)", Style::default()));
        assert_eq!(anchor, "top");
        let same = plain(&inline("[https://a.dev](https://a.dev)", Style::default()));
        assert_eq!(same, "https://a.dev");
        assert_eq!(
            plain(&inline("![a cat](c.png)", Style::default())),
            "[image: a cat]"
        );
        let bare = inline("go to https://example.com/x, then", Style::default());
        assert_eq!(plain(&bare), "go to https://example.com/x, then");
        assert!(
            bare.iter().any(|(t, s)| t == "https://example.com/x"
                && s.add_modifier.contains(Modifier::UNDERLINED))
        );
        let long = plain(&inline(
            &format!("[x](https://e.com/{})", "a".repeat(80)),
            Style::default(),
        ));
        assert!(long.contains('…') && long.len() < 80);
    }

    #[test]
    fn code_spans_keep_markdown_literal() {
        let t = plain(&inline(
            "use `a **b** _c_` here and ``tick ` inside``",
            Style::default(),
        ));
        assert_eq!(t, "use  a **b** _c_  here and  tick ` inside ");
    }

    #[test]
    fn headings_rules_tasks_and_nested_bullets() {
        let r = render(
            "# Title\n\n## Sub\n\n---\n\n- [ ] todo\n- [x] done\n- top\n  - nested\n    - deeper",
            60,
        );
        assert!(r.iter().any(|l| l.trim() == "Title"));
        assert!(
            r.iter().any(|l| l.contains('━')),
            "h1 gets an underline rule"
        );
        assert!(
            r.iter().any(|l| l.trim_start().starts_with("─")),
            "horizontal rule"
        );
        assert!(r.iter().any(|l| l.contains("☐ todo")) && r.iter().any(|l| l.contains("☑ done")));
        assert!(r.iter().any(|l| l.contains("• top")));
        assert!(r.iter().any(|l| l.contains("◦ nested")));
        assert!(r.iter().any(|l| l.contains("▪ deeper")));
    }

    #[test]
    fn tables_render_aligned_and_wrap_to_width() {
        let md = "| Name | Qty | Notes |\n|:-----|----:|:-----:|\n| apples | 3 | fresh |\n| kiwi | 12 | a much longer note that must wrap inside its column |\n";
        let wide = render(md, 100);
        assert!(wide[0].contains('┌'));
        assert!(
            wide.iter()
                .rev()
                .find(|l| !l.is_empty())
                .unwrap()
                .contains('└')
        );
        assert!(wide.iter().any(|l| l.contains("Name") && l.contains("Qty")));
        assert!(wide.iter().any(|l| l.contains('┼')), "header separator");
        let apples = wide.iter().find(|l| l.contains("apples")).unwrap();
        assert!(
            apples.contains("│ apples") && apples.contains("3 │"),
            "left/right alignment: {apples}"
        );
        for w in [100usize, 60, 40, 30, 24] {
            for l in render(md, w) {
                assert!(l.width() <= w, "width {w}: row is {} wide: {l}", l.width());
            }
        }
        // the long note wrapped onto several rows rather than being cut
        let narrow = render(md, 50).join("\n");
        assert!(
            narrow.contains("wrap") && narrow.contains("column"),
            "{narrow}"
        );
        // too narrow for a grid: degrades to plain lines, still inside the width
        for l in render(md, 14) {
            assert!(l.width() <= 14, "{l}");
        }
    }

    #[test]
    fn a_lone_dash_line_is_a_rule_not_a_table() {
        let r = render("a | b\n---\nnext", 40);
        assert!(r.iter().any(|l| l.trim_start().starts_with('─')));
        assert!(!r.iter().any(|l| l.contains('┌')));
    }

    #[test]
    fn nested_quotes_show_one_bar_per_level() {
        let r = render("> outer\n>> inner", 40);
        assert!(r[0].contains("▎ outer") && !r[0].contains("▎▎"));
        assert!(r[1].contains("▎▎ inner"));
    }

    #[test]
    fn mixed_document_never_overflows() {
        let doc = "# H\ntext with **bold** and a very_long_identifier_name_that_never_ends_ok and https://example.com/a/very/long/path/that/keeps/going/and/going\n- item [link](https://example.com/x)\n\n```rust\nfn main() {}\n```\n";
        for w in [80usize, 40, 20] {
            for l in render(doc, w) {
                assert!(l.width() <= w, "{w}: {l}");
            }
        }
    }
}
