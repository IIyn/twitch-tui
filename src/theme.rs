//! Color palette.

use term::{Rgb, Style};

pub const BG: Rgb = Rgb(14, 14, 18);
pub const PANEL: Rgb = Rgb(20, 20, 26);
pub const HIGHLIGHT: Rgb = Rgb(38, 32, 58);
pub const BORDER: Rgb = Rgb(52, 50, 66);
pub const TEXT: Rgb = Rgb(239, 239, 241);
pub const MUTED: Rgb = Rgb(140, 138, 158);
pub const FAINT: Rgb = Rgb(84, 82, 100);
pub const ACCENT: Rgb = Rgb(145, 70, 255);
pub const ACCENT_SOFT: Rgb = Rgb(191, 148, 255);
pub const LIVE: Rgb = Rgb(235, 4, 0);
pub const PINK: Rgb = Rgb(255, 92, 170);
pub const GREEN: Rgb = Rgb(0, 214, 143);
pub const YELLOW: Rgb = Rgb(255, 196, 107);
#[cfg(target_os = "linux")]
pub const CYAN: Rgb = Rgb(94, 224, 255);

const STOPS: [Rgb; 5] = [
    Rgb(91, 43, 217),
    Rgb(145, 70, 255),
    Rgb(225, 91, 255),
    Rgb(255, 111, 168),
    Rgb(255, 196, 107),
];

/// Signature purple → pink → amber gradient, `t` in 0..=1.
pub fn gradient(t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0) * (STOPS.len() - 1) as f32;
    let i = (t.floor() as usize).min(STOPS.len() - 2);
    STOPS[i].lerp(STOPS[i + 1], t - i as f32)
}

pub fn base() -> Style {
    Style::new(TEXT, BG)
}

pub fn panel() -> Style {
    Style::new(TEXT, PANEL)
}

/// Makes a user-chosen chat color readable on the dark background.
#[cfg(target_os = "linux")]
pub fn readable(c: Rgb) -> Rgb {
    let luma = 0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32;
    if luma < 90.0 { c.lerp(Rgb(255, 255, 255), (90.0 - luma) / 160.0 + 0.2) } else { c }
}

/// Stable fallback color for users without a chat color.
#[cfg(target_os = "linux")]
pub fn name_color(name: &str) -> Rgb {
    const COLORS: [Rgb; 8] = [
        Rgb(255, 127, 80),
        Rgb(30, 144, 255),
        Rgb(50, 205, 50),
        Rgb(218, 112, 214),
        Rgb(255, 215, 0),
        Rgb(0, 206, 209),
        Rgb(255, 105, 180),
        Rgb(154, 205, 50),
    ];
    let hash = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    COLORS[hash as usize % COLORS.len()]
}
