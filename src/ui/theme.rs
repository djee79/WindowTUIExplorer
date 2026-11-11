//! Colors: one palette per colorscheme (`:colorscheme`, kept in ui.toml). Every UI color
//! comes from [`pal`], so a scheme switch shows on the next frame.

use ratatui::style::{Color, Modifier, Style};
use std::sync::atomic::{AtomicUsize, Ordering};

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The colors the UI draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    /// Secondary text: dates, hints, notes.
    pub dim: Color,
    pub border: Color,
    /// The cursor's line number, key chips, titles.
    pub accent: Color,
    pub dir: Color,
    /// UNC paths and shares.
    pub net: Color,
    pub ok: Color,
    pub err: Color,
    pub warn: Color,
    pub sel_bg: Color,
    pub sel_fg: Color,
    /// The selection in a column that doesn't have focus.
    pub inactive_sel_bg: Color,
    /// Popups, pickers, panels.
    pub panel_bg: Color,
    pub bar_bg: Color,
    /// Fainter than `dim`: relative numbers, gaps.
    pub dimmed: Color,
    /// Text on an accent background.
    pub on_accent: Color,
    /// `:` commands (the `g?` command column).
    pub cmd: Color,
}

const DEFAULT: Palette = Palette {
    bg: rgb(0x15171c),
    fg: rgb(0xc8cdd5),
    dim: rgb(0x858c97),
    border: rgb(0x3b414c),
    accent: rgb(0xe3a857),
    dir: rgb(0x7fa6f5),
    net: rgb(0x6ccbc4),
    ok: rgb(0x86c98a),
    err: rgb(0xf0817a),
    warn: rgb(0xe9d06c),
    sel_bg: rgb(0x2b313b),
    sel_fg: rgb(0xeef1f5),
    inactive_sel_bg: rgb(0x22262e),
    panel_bg: rgb(0x1c1f25),
    bar_bg: rgb(0x1d2026),
    dimmed: rgb(0x4b515b),
    on_accent: rgb(0x15171c),
    cmd: rgb(0xb89af0),
};

/// Tokyo Night (folke/tokyonight.nvim), night: blue accent, cyan folders, teal shares.
const TOKYONIGHT_NIGHT: Palette = Palette {
    bg: rgb(0x1a1b26),
    fg: rgb(0xc0caf5),
    dim: rgb(0x737aa2),
    border: rgb(0x3b4261),
    accent: rgb(0x7aa2f7),
    dir: rgb(0x7dcfff),
    net: rgb(0x1abc9c),
    ok: rgb(0x9ece6a),
    err: rgb(0xf7768e),
    warn: rgb(0xe0af68),
    sel_bg: rgb(0x283457),
    sel_fg: rgb(0xc0caf5),
    inactive_sel_bg: rgb(0x292e42),
    panel_bg: rgb(0x16161e),
    bar_bg: rgb(0x16161e),
    dimmed: rgb(0x565f89),
    on_accent: rgb(0x16161e),
    cmd: rgb(0xbb9af7),
};

/// Tokyo Night storm: night's colors on a bluer, lighter background.
const TOKYONIGHT_STORM: Palette = Palette {
    bg: rgb(0x24283b),
    sel_bg: rgb(0x2e3c64),
    panel_bg: rgb(0x1f2335),
    bar_bg: rgb(0x1f2335),
    on_accent: rgb(0x1f2335),
    ..TOKYONIGHT_NIGHT
};

/// Tokyo Night moon: softer, warmer colors.
const TOKYONIGHT_MOON: Palette = Palette {
    bg: rgb(0x222436),
    fg: rgb(0xc8d3f5),
    dim: rgb(0x828bb8),
    border: rgb(0x3b4261),
    accent: rgb(0x82aaff),
    dir: rgb(0x86e1fc),
    net: rgb(0x4fd6be),
    ok: rgb(0xc3e88d),
    err: rgb(0xff757f),
    warn: rgb(0xffc777),
    sel_bg: rgb(0x2d3f76),
    sel_fg: rgb(0xc8d3f5),
    inactive_sel_bg: rgb(0x2f334d),
    panel_bg: rgb(0x1e2030),
    bar_bg: rgb(0x1e2030),
    dimmed: rgb(0x636da6),
    on_accent: rgb(0x1e2030),
    cmd: rgb(0xc099ff),
};

/// The schemes, by name; the first is the default.
pub const SCHEMES: &[(&str, Palette)] = &[
    ("default", DEFAULT),
    ("tokyonight-night", TOKYONIGHT_NIGHT),
    ("tokyonight-storm", TOKYONIGHT_STORM),
    ("tokyonight-moon", TOKYONIGHT_MOON),
];

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// The palette in use.
pub fn pal() -> &'static Palette {
    let i = ACTIVE.load(Ordering::Relaxed);
    &SCHEMES.get(i).unwrap_or(&SCHEMES[0]).1
}

/// The scheme's full name: `tokyonight` is night, as in Neovim; case doesn't matter.
pub fn scheme_name(name: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    let n = if n == "tokyonight" {
        "tokyonight-night"
    } else {
        n.as_str()
    };
    SCHEMES.iter().find(|(s, _)| *s == n).map(|(s, _)| *s)
}

/// Switches to `name` (see [`scheme_name`]); the name it switched to, or None if unknown.
pub fn set_scheme(name: &str) -> Option<&'static str> {
    let full = scheme_name(name)?;
    let i = SCHEMES.iter().position(|(s, _)| *s == full)?;
    ACTIVE.store(i, Ordering::Relaxed);
    Some(full)
}

/// The scheme in use.
pub fn scheme() -> &'static str {
    SCHEMES
        .get(ACTIVE.load(Ordering::Relaxed))
        .map_or(SCHEMES[0].0, |(s, _)| *s)
}

pub fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

pub fn dim() -> Style {
    fg(pal().dim)
}

pub fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

/// `"#e3a857"` / `"e3a857"` → a colour; anything else → None.
pub fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    u32::from_str_radix(h, 16).ok().map(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_names() {
        assert_eq!(scheme_name("tokyonight"), Some("tokyonight-night"));
        assert_eq!(scheme_name(" TokyoNight-Moon "), Some("tokyonight-moon"));
        assert_eq!(scheme_name("default"), Some("default"));
        assert_eq!(scheme_name("gruvbox"), None);
        // Storm differs from night only in its backgrounds.
        assert_eq!(TOKYONIGHT_STORM.accent, TOKYONIGHT_NIGHT.accent);
        assert_ne!(TOKYONIGHT_STORM.bg, TOKYONIGHT_NIGHT.bg);
    }
}
