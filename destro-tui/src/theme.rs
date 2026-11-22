//! Colour themes. One global "current theme" so layout code (markdown, syntax
//! highlighting, panels) can ask for a colour without threading state around.
//! Switching themes must invalidate cached transcript lines (the cache bakes
//! colours in) — `App::refresh_theme` does that.

use ratatui::style::{Color, Modifier, Style};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Theme {
    pub name: &'static str,
    /// App background. `Color::Reset` keeps the terminal's own background.
    pub bg: Color,
    /// Side panels and popups, a step away from `bg`.
    pub surface: Color,
    pub fg: Color,
    pub muted: Color,
    /// Borders, rules, gutters.
    pub faint: Color,
    pub accent: Color,
    pub accent2: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    /// Selected row / user bubble background.
    pub sel: Color,
    /// Code block background.
    pub code: Color,
    /// Text drawn on top of an accent-coloured chip.
    pub on_accent: Color,
    pub kw: Color,
    pub str_: Color,
    pub num: Color,
    pub ty: Color,
    pub func: Color,
}

const fn hx(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

pub static THEMES: [Theme; 10] = [
    Theme {
        name: "destro",
        bg: hx(0x0e0f13),
        surface: hx(0x14161c),
        fg: hx(0xe8e6e3),
        muted: hx(0x7d818c),
        faint: hx(0x2a2d36),
        accent: hx(0xff5f3c),
        accent2: hx(0xffb347),
        ok: hx(0x7fd18b),
        warn: hx(0xffcc66),
        err: hx(0xff5c6c),
        sel: hx(0x232733),
        code: hx(0x171a21),
        on_accent: hx(0x0e0f13),
        kw: hx(0xff6b8a),
        str_: hx(0x8fd694),
        num: hx(0xffb86b),
        ty: hx(0x6cc6d9),
        func: hx(0x7aa2f7),
    },
    Theme {
        name: "tokyo-night",
        bg: hx(0x1a1b26),
        surface: hx(0x16161e),
        fg: hx(0xc0caf5),
        muted: hx(0x565f89),
        faint: hx(0x2f334d),
        accent: hx(0x7aa2f7),
        accent2: hx(0xbb9af7),
        ok: hx(0x9ece6a),
        warn: hx(0xe0af68),
        err: hx(0xf7768e),
        sel: hx(0x283457),
        code: hx(0x1f2335),
        on_accent: hx(0x1a1b26),
        kw: hx(0xbb9af7),
        str_: hx(0x9ece6a),
        num: hx(0xff9e64),
        ty: hx(0x2ac3de),
        func: hx(0x7aa2f7),
    },
    Theme {
        name: "catppuccin",
        bg: hx(0x1e1e2e),
        surface: hx(0x181825),
        fg: hx(0xcdd6f4),
        muted: hx(0x7f849c),
        faint: hx(0x45475a),
        accent: hx(0xcba6f7),
        accent2: hx(0x89b4fa),
        ok: hx(0xa6e3a1),
        warn: hx(0xf9e2af),
        err: hx(0xf38ba8),
        sel: hx(0x313244),
        code: hx(0x11111b),
        on_accent: hx(0x1e1e2e),
        kw: hx(0xcba6f7),
        str_: hx(0xa6e3a1),
        num: hx(0xfab387),
        ty: hx(0xf9e2af),
        func: hx(0x89b4fa),
    },
    Theme {
        name: "latte",
        bg: hx(0xeff1f5),
        surface: hx(0xe6e9ef),
        fg: hx(0x4c4f69),
        muted: hx(0x8c8fa1),
        faint: hx(0xbcc0cc),
        accent: hx(0x8839ef),
        accent2: hx(0x1e66f5),
        ok: hx(0x40a02b),
        warn: hx(0xdf8e1d),
        err: hx(0xd20f39),
        sel: hx(0xccd0da),
        code: hx(0xe6e9ef),
        on_accent: hx(0xeff1f5),
        kw: hx(0x8839ef),
        str_: hx(0x40a02b),
        num: hx(0xfe640b),
        ty: hx(0xdf8e1d),
        func: hx(0x1e66f5),
    },
    Theme {
        name: "gruvbox",
        bg: hx(0x282828),
        surface: hx(0x1d2021),
        fg: hx(0xebdbb2),
        muted: hx(0x928374),
        faint: hx(0x504945),
        accent: hx(0xfabd2f),
        accent2: hx(0xfe8019),
        ok: hx(0xb8bb26),
        warn: hx(0xfabd2f),
        err: hx(0xfb4934),
        sel: hx(0x3c3836),
        code: hx(0x1d2021),
        on_accent: hx(0x282828),
        kw: hx(0xfb4934),
        str_: hx(0xb8bb26),
        num: hx(0xd3869b),
        ty: hx(0xfabd2f),
        func: hx(0x83a598),
    },
    Theme {
        name: "nord",
        bg: hx(0x2e3440),
        surface: hx(0x292e39),
        fg: hx(0xd8dee9),
        muted: hx(0x7b88a1),
        faint: hx(0x434c5e),
        accent: hx(0x88c0d0),
        accent2: hx(0xb48ead),
        ok: hx(0xa3be8c),
        warn: hx(0xebcb8b),
        err: hx(0xbf616a),
        sel: hx(0x3b4252),
        code: hx(0x272c36),
        on_accent: hx(0x2e3440),
        kw: hx(0x81a1c1),
        str_: hx(0xa3be8c),
        num: hx(0xb48ead),
        ty: hx(0x8fbcbb),
        func: hx(0x88c0d0),
    },
    Theme {
        name: "dracula",
        bg: hx(0x282a36),
        surface: hx(0x21222c),
        fg: hx(0xf8f8f2),
        muted: hx(0x6272a4),
        faint: hx(0x44475a),
        accent: hx(0xbd93f9),
        accent2: hx(0xff79c6),
        ok: hx(0x50fa7b),
        warn: hx(0xf1fa8c),
        err: hx(0xff5555),
        sel: hx(0x44475a),
        code: hx(0x21222c),
        on_accent: hx(0x282a36),
        kw: hx(0xff79c6),
        str_: hx(0xf1fa8c),
        num: hx(0xbd93f9),
        ty: hx(0x8be9fd),
        func: hx(0x50fa7b),
    },
    Theme {
        name: "rose-pine",
        bg: hx(0x191724),
        surface: hx(0x1f1d2e),
        fg: hx(0xe0def4),
        muted: hx(0x6e6a86),
        faint: hx(0x403d52),
        accent: hx(0xc4a7e7),
        accent2: hx(0xebbcba),
        ok: hx(0x9ccfd8),
        warn: hx(0xf6c177),
        err: hx(0xeb6f92),
        sel: hx(0x26233a),
        code: hx(0x1f1d2e),
        on_accent: hx(0x191724),
        kw: hx(0xeb6f92),
        str_: hx(0xf6c177),
        num: hx(0xebbcba),
        ty: hx(0x9ccfd8),
        func: hx(0xc4a7e7),
    },
    Theme {
        name: "mono",
        bg: hx(0x111111),
        surface: hx(0x161616),
        fg: hx(0xe6e6e6),
        muted: hx(0x8a8a8a),
        faint: hx(0x333333),
        accent: hx(0xffffff),
        accent2: hx(0xbdbdbd),
        ok: hx(0xd0d0d0),
        warn: hx(0xf2f2f2),
        err: hx(0xffffff),
        sel: hx(0x2a2a2a),
        code: hx(0x181818),
        on_accent: hx(0x111111),
        kw: hx(0xffffff),
        str_: hx(0xbdbdbd),
        num: hx(0xd9d9d9),
        ty: hx(0xa6a6a6),
        func: hx(0xe6e6e6),
    },
    // Follows whatever palette the terminal itself is configured with.
    Theme {
        name: "terminal",
        bg: Color::Reset,
        surface: Color::Reset,
        fg: Color::Reset,
        muted: Color::DarkGray,
        faint: Color::DarkGray,
        accent: Color::Cyan,
        accent2: Color::Magenta,
        ok: Color::Green,
        warn: Color::Yellow,
        err: Color::Red,
        sel: Color::Indexed(238),
        code: Color::Indexed(236),
        on_accent: Color::Black,
        kw: Color::Magenta,
        str_: Color::Green,
        num: Color::Yellow,
        ty: Color::Cyan,
        func: Color::LightBlue,
    },
];

static CUR: AtomicUsize = AtomicUsize::new(0);

pub fn cur() -> &'static Theme {
    &THEMES[CUR.load(Ordering::Relaxed) % THEMES.len()]
}

pub fn index() -> usize {
    CUR.load(Ordering::Relaxed) % THEMES.len()
}

pub fn set_index(i: usize) {
    CUR.store(i % THEMES.len(), Ordering::Relaxed);
}

/// Select by (case-insensitive) name; false when unknown.
pub fn set_by_name(name: &str) -> bool {
    let n = name.trim().to_lowercase().replace([' ', '_'], "-");
    match THEMES.iter().position(|t| t.name == n) {
        Some(i) => {
            set_index(i);
            true
        }
        None => false,
    }
}

pub fn names() -> String {
    THEMES.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
}

/// Linear blend of two RGB colours (falls back to `a` for palette colours).
pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
        }
        _ => a,
    }
}

// ----- style helpers ---------------------------------------------------------------------

pub fn base() -> Style {
    let t = cur();
    Style::new().fg(t.fg).bg(t.bg)
}
pub fn surface() -> Style {
    let t = cur();
    Style::new().fg(t.fg).bg(t.surface)
}
pub fn text() -> Style {
    Style::new().fg(cur().fg)
}
pub fn bold() -> Style {
    text().add_modifier(Modifier::BOLD)
}
pub fn muted() -> Style {
    Style::new().fg(cur().muted)
}
pub fn faint() -> Style {
    Style::new().fg(cur().faint)
}
pub fn accent() -> Style {
    Style::new().fg(cur().accent)
}
pub fn accent_b() -> Style {
    accent().add_modifier(Modifier::BOLD)
}
pub fn accent2() -> Style {
    Style::new().fg(cur().accent2)
}
pub fn ok() -> Style {
    Style::new().fg(cur().ok)
}
pub fn warn() -> Style {
    Style::new().fg(cur().warn)
}
pub fn err() -> Style {
    Style::new().fg(cur().err)
}
pub fn sel() -> Style {
    Style::new().bg(cur().sel)
}
pub fn code_bg() -> Style {
    Style::new().bg(cur().code)
}
/// Filled label: dark text on a coloured background.
pub fn chip(bg: Color) -> Style {
    Style::new()
        .fg(cur().on_accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_selectable() {
        for (i, t) in THEMES.iter().enumerate() {
            assert_eq!(THEMES.iter().filter(|o| o.name == t.name).count(), 1);
            assert!(set_by_name(t.name));
            assert_eq!(index(), i);
        }
        assert!(!set_by_name("nope"));
        set_index(0);
    }
}
