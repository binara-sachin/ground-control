//! Theme tokens (§10.1). Colour roles are defined once here; status is never
//! shown by colour alone — callers pair every colour with a word or symbol.

use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(0x1b, 0x1f, 0x27);
pub const BAR_BG: Color = Color::Rgb(0x24, 0x29, 0x33);
pub const TEXT: Color = Color::Rgb(0xd8, 0xdc, 0xe4);
pub const MUTED: Color = Color::Rgb(0x83, 0x8b, 0x99);
const DIM: Color = Color::Rgb(0x45, 0x4c, 0x5a);
pub const ACCENT: Color = Color::Rgb(0xe5, 0xb5, 0x67);
pub const SUCCESS: Color = Color::Rgb(0xa6, 0xcc, 0x70);
pub const WARNING: Color = Color::Rgb(0xe5, 0xb5, 0x67);
pub const DANGER: Color = Color::Rgb(0xf0, 0x71, 0x78);
pub const INFO: Color = Color::Rgb(0x82, 0xaa, 0xff);
pub const SELECTION: Color = Color::Rgb(0x2c, 0x3a, 0x52);
pub const ON_ACCENT: Color = Color::Rgb(0x1b, 0x1f, 0x27);

/// The 16-colour ANSI palette used to draw captured output; `None` past index 15.
pub fn ansi(index: u8) -> Option<Color> {
    Some(match index {
        0 => Color::Rgb(0x3b, 0x41, 0x50),
        1 => DANGER,
        2 => SUCCESS,
        3 => WARNING,
        4 => INFO,
        5 => Color::Rgb(0xc7, 0x92, 0xea),
        6 => Color::Rgb(0x89, 0xdd, 0xff),
        7 => TEXT,
        8 => MUTED,
        9 => Color::Rgb(0xff, 0x8b, 0x92),
        10 => Color::Rgb(0xc3, 0xe8, 0x8d),
        11 => Color::Rgb(0xff, 0xcb, 0x6b),
        12 => Color::Rgb(0x9c, 0xc4, 0xff),
        13 => Color::Rgb(0xdd, 0xa8, 0xff),
        14 => Color::Rgb(0xa3, 0xf7, 0xff),
        15 => Color::White,
        _ => return None,
    })
}

/// Striped accent used on approval banners and protected dialogs.
pub const STRIPES: &str = "▚▚▚";
/// Marker for protected pipelines and environments.
pub const PROTECTED: &str = "▞";

pub fn base() -> Style {
    Style::new().fg(TEXT).bg(BG)
}
pub fn text() -> Style {
    Style::new().fg(TEXT)
}
pub fn muted() -> Style {
    Style::new().fg(MUTED)
}
pub fn dim() -> Style {
    Style::new().fg(DIM)
}
pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}
pub fn bold() -> Style {
    Style::new().fg(TEXT).add_modifier(Modifier::BOLD)
}
pub fn success() -> Style {
    Style::new().fg(SUCCESS)
}
pub fn warning() -> Style {
    Style::new().fg(WARNING)
}
pub fn danger() -> Style {
    Style::new().fg(DANGER)
}
pub fn info() -> Style {
    Style::new().fg(INFO)
}
pub fn key() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}
pub fn selected() -> Style {
    Style::new().bg(SELECTION)
}
