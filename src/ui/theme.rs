//! Colors. Mirrors `docs/theme.toml`; loading that file at runtime comes with config.

use ratatui::style::{Color, Modifier, Style};

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const BG: Color = rgb(0x15171c);
pub const FG: Color = rgb(0xc8cdd5);
pub const DIM: Color = rgb(0x858c97);
pub const BORDER: Color = rgb(0x3b414c);
pub const ACCENT: Color = rgb(0xe3a857);
pub const DIR: Color = rgb(0x7fa6f5);
pub const NET: Color = rgb(0x6ccbc4);
pub const OK: Color = rgb(0x86c98a);
pub const ERR: Color = rgb(0xf0817a);
pub const WARN: Color = rgb(0xe9d06c);
pub const SEL_BG: Color = rgb(0x2b313b);
pub const SEL_FG: Color = rgb(0xeef1f5);
pub const INACTIVE_SEL_BG: Color = rgb(0x22262e);
pub const PANEL_BG: Color = rgb(0x1c1f25);
pub const BAR_BG: Color = rgb(0x1d2026);
pub const DIMMED: Color = rgb(0x4b515b);
pub const ON_ACCENT: Color = rgb(0x15171c);

pub fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

pub fn dim() -> Style {
    fg(DIM)
}

pub fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}
