//! "Halation" as ratatui styles — one palette, two renderers.
//!
//! The print path (`albedo.rs`, `albedo/printer.rs`, the server's `timing.rs`)
//! writes raw SGR escapes; the dashboard composes `ratatui::Style`s. Both draw
//! from the indices below so a colour can't drift between the two surfaces —
//! the same failure the tier-report padding bug came from, one layer up.
//!
//! The idea the palette encodes: **a tier reads as brightness.** Tier A is
//! settled, static light and sits deep in the ramp; Tier C is a live island and
//! burns at the top. That is why the ramp is luminance-ordered rather than
//! hue-coded — the mix of a build is legible before any label is read.

use ratatui::style::{Color, Modifier, Style};

/// Champagne gold — glyphs, headings, the primary mark.
pub const ACCENT: Color = Color::Indexed(179);
/// Pale gold / cream — values, live state, the hero number.
pub const ACCENT_SOFT: Color = Color::Indexed(223);
/// Deep gold — dividers, borders, secondary marks.
pub const ACCENT_DEEP: Color = Color::Indexed(137);
/// Warm-neutral gray — labels and anything deliberately quiet.
pub const MUTED: Color = Color::Indexed(245);

pub const OK: Color = Color::Indexed(35);
pub const ERR: Color = Color::Indexed(174);

/// Vertical glow used by the wordmark: cream at the crown, cooling to deep gold
/// in the drop shadow. Light catching the top edge of the letters.
pub const BRAND_RAMP: [Color; 6] = [
    Color::Indexed(137),
    Color::Indexed(179),
    Color::Indexed(221),
    Color::Indexed(222),
    Color::Indexed(223),
    Color::Indexed(230),
];

/// Tier luminance: A settled, B hydrated, C live. Mirrors `printer.rs`'s
/// `TIER_LUMEN` exactly.
pub const TIER_RAMP: [Color; 3] = [
    Color::Indexed(137),
    Color::Indexed(179),
    Color::Indexed(222),
];

pub fn accent() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn value() -> Style {
    Style::default()
        .fg(ACCENT_SOFT)
        .add_modifier(Modifier::BOLD)
}

pub fn label() -> Style {
    Style::default().fg(MUTED)
}

pub fn dim() -> Style {
    Style::default()
        .fg(MUTED)
        .add_modifier(Modifier::DIM)
}

pub fn border() -> Style {
    Style::default().fg(ACCENT_DEEP)
}

pub fn tier(index: usize) -> Style {
    Style::default()
        .fg(TIER_RAMP[index.min(2)])
        .add_modifier(Modifier::BOLD)
}

/// A horizontal luminance bar — `filled` of `width` cells lit at the tier's
/// brightness, the remainder left as unlit track.
pub fn bar(filled: usize, width: usize) -> (String, String) {
    let filled = filled.min(width);
    ("█".repeat(filled), "░".repeat(width - filled))
}
