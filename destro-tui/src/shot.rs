//! Screenshots: render a frame of the real UI to a terminal-window SVG.
//!
//! `destro --snapshot <scene> --svg` draws the actual widgets into an in-memory
//! buffer and writes every cell (glyph, colours, weight) out as SVG. That is how
//! the README images are made, so they always show what the app really renders,
//! in whichever theme is selected.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

use crate::app::App;
use crate::{theme, ui};

const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.0;
const FONT: f32 = 14.0;
const PAD: f32 = 18.0;
const BAR: f32 = 36.0;

type Rgb = (u8, u8, u8);

fn rgb(c: Color, fallback: Rgb) -> Rgb {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (12, 12, 12),
        Color::Red => (197, 15, 31),
        Color::Green => (19, 161, 14),
        Color::Yellow => (193, 156, 0),
        Color::Blue => (0, 55, 218),
        Color::Magenta => (136, 23, 152),
        Color::Cyan => (58, 150, 221),
        Color::Gray => (204, 204, 204),
        Color::DarkGray => (118, 118, 118),
        Color::LightRed => (231, 72, 86),
        Color::LightGreen => (22, 198, 12),
        Color::LightYellow => (249, 241, 165),
        Color::LightBlue => (59, 120, 255),
        Color::LightMagenta => (180, 0, 158),
        Color::LightCyan => (97, 214, 214),
        Color::White => (242, 242, 242),
        Color::Indexed(n @ 232..=255) => {
            let v = 8 + 10 * (n - 232);
            (v, v, v)
        }
        _ => fallback,
    }
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Clone, Copy, PartialEq)]
struct Look {
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

/// One frame of `app` at `w` x `h` cells as an SVG document.
pub fn svg(app: &mut App, w: u16, h: u16, title: &str) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
    term.draw(|f| ui::draw(f, app)).expect("draw");
    let buf = term.backend().buffer().clone();

    let th = theme::cur();
    let dbg = rgb(th.bg, (12, 12, 12));
    let dfg = rgb(th.fg, (204, 204, 204));
    let chrome = rgb(th.surface, (30, 30, 30));
    let width = w as f32 * CELL_W + PAD * 2.0;
    let height = h as f32 * CELL_H + BAR + PAD;

    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {width:.0} {height:.0}\">\n"
    ));
    out.push_str(&format!(
        "<rect width=\"{width:.0}\" height=\"{height:.0}\" rx=\"12\" fill=\"{}\"/>\n",
        hex(dbg)
    ));
    // window bar
    out.push_str(&format!(
        "<path d=\"M0 12 a12 12 0 0 1 12 -12 h{:.0} a12 12 0 0 1 12 12 v{:.0} h-{width:.0} z\" fill=\"{}\"/>\n",
        width - 24.0,
        BAR - 12.0,
        hex(chrome)
    ));
    for (i, c) in ["#ff5f57", "#febc2e", "#28c840"].iter().enumerate() {
        out.push_str(&format!(
            "<circle cx=\"{:.0}\" cy=\"{:.0}\" r=\"6\" fill=\"{c}\"/>\n",
            22.0 + i as f32 * 20.0,
            BAR / 2.0
        ));
    }
    out.push_str(&format!(
        "<text x=\"{:.0}\" y=\"{:.0}\" fill=\"{}\" font-family=\"Segoe UI, Helvetica, Arial, sans-serif\" font-size=\"13\" text-anchor=\"middle\" opacity=\"0.7\">{}</text>\n",
        width / 2.0,
        BAR / 2.0 + 4.5,
        hex(dfg),
        esc(title)
    ));
    out.push_str(&format!(
        "<g font-family=\"Cascadia Mono, Cascadia Code, Consolas, DejaVu Sans Mono, monospace\" font-size=\"{FONT}\">\n"
    ));

    for y in 0..h {
        let top = BAR + y as f32 * CELL_H;
        // backgrounds: merge runs of the same colour
        let mut run: Option<(u16, Rgb)> = None;
        let flush_bg = |out: &mut String, run: Option<(u16, Rgb)>, end: u16| {
            if let Some((start, c)) = run {
                out.push_str(&format!(
                    "<rect x=\"{:.1}\" y=\"{top:.1}\" width=\"{:.1}\" height=\"{CELL_H}\" fill=\"{}\"/>\n",
                    PAD + start as f32 * CELL_W,
                    (end - start) as f32 * CELL_W,
                    hex(c)
                ));
            }
        };
        for x in 0..w {
            let cell = buf.cell((x, y)).expect("cell");
            let rev = cell.modifier.contains(Modifier::REVERSED);
            let bg = if rev {
                rgb(cell.fg, dfg)
            } else {
                rgb(cell.bg, dbg)
            };
            let bg = if bg == dbg { None } else { Some(bg) };
            match (run, bg) {
                (Some((_, c)), Some(b)) if c == b => {}
                (Some(_), _) => {
                    flush_bg(&mut out, run, x);
                    run = bg.map(|b| (x, b));
                }
                (None, Some(b)) => run = Some((x, b)),
                (None, None) => {}
            }
        }
        flush_bg(&mut out, run, w);

        // text: merge runs of the same look
        let mut text = String::new();
        let mut start = 0u16;
        let mut look: Option<Look> = None;
        let flush_text = |out: &mut String, text: &mut String, start: u16, look: Option<Look>| {
            let Some(l) = look else { return };
            let n = text.chars().count();
            let lead = text.chars().take_while(|c| *c == ' ').count();
            let trail = text.chars().rev().take_while(|c| *c == ' ').count();
            if lead + trail >= n {
                text.clear();
                return;
            }
            // braille spinner frames fall back to an odd glyph in browser fonts
            let body: String = text
                .chars()
                .skip(lead)
                .take(n - lead - trail)
                .map(|c| {
                    if ('\u{2800}'..='\u{28ff}').contains(&c) {
                        '●'
                    } else {
                        c
                    }
                })
                .collect();
            let len = (n - lead - trail) as f32 * CELL_W;
            let mut attrs = String::new();
            if l.bold {
                attrs.push_str(" font-weight=\"bold\"");
            }
            if l.italic {
                attrs.push_str(" font-style=\"italic\"");
            }
            match (l.underline, l.strike) {
                (true, true) => attrs.push_str(" text-decoration=\"underline line-through\""),
                (true, false) => attrs.push_str(" text-decoration=\"underline\""),
                (false, true) => attrs.push_str(" text-decoration=\"line-through\""),
                _ => {}
            }
            out.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{}\" textLength=\"{len:.1}\" lengthAdjust=\"spacing\" xml:space=\"preserve\"{attrs}>{}</text>\n",
                PAD + (start as f32 + lead as f32) * CELL_W,
                top + 13.0,
                hex(l.fg),
                esc(&body)
            ));
            text.clear();
        };
        for x in 0..w {
            let cell = buf.cell((x, y)).expect("cell");
            let rev = cell.modifier.contains(Modifier::REVERSED);
            let fg = if rev {
                rgb(cell.bg, dbg)
            } else {
                rgb(cell.fg, dfg)
            };
            let l = Look {
                fg,
                bold: cell.modifier.contains(Modifier::BOLD),
                italic: cell.modifier.contains(Modifier::ITALIC),
                underline: cell.modifier.contains(Modifier::UNDERLINED),
                strike: cell.modifier.contains(Modifier::CROSSED_OUT),
            };
            if look != Some(l) {
                flush_text(&mut out, &mut text, start, look);
                look = Some(l);
                start = x;
            }
            text.push_str(cell.symbol());
        }
        flush_text(&mut out, &mut text, start, look);
    }
    out.push_str("</g>\n</svg>\n");
    out
}
