//! Visual tokens for the switcher panel.
//!
//! Kept intentionally small: the whole UI reads from a single struct so theming
//! is just "swap a `Theme`". Wired to TOML config in a later iteration.

use gpui::{hsla, rgb, Hsla, Rgba};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub background: Rgba,
    pub foreground: Rgba,
    pub muted: Rgba,
    pub accent: Rgba,
    pub selection: Rgba,
    pub match_highlight: Hsla,
    pub border: Rgba,
    /// Red used for irreversible actions (e.g. Quit button, delete row).
    pub destructive: Rgba,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            background: rgb(0x1e1e24),
            foreground: rgb(0xf0f0f0),
            muted: rgb(0x8a8a94),
            accent: rgb(0x7aa2f7),
            selection: rgb(0x2e3440),
            match_highlight: hsla(50.0 / 360.0, 0.95, 0.65, 1.0),
            border: rgb(0x2d2d35),
            destructive: rgb(0xef4444),
        }
    }

    pub fn light() -> Self {
        Self {
            background: rgb(0xf7f7f8),
            foreground: rgb(0x1a1a1a),
            muted: rgb(0x6b6b73),
            accent: rgb(0x2563eb),
            selection: rgb(0xdbeafe),
            match_highlight: hsla(25.0 / 360.0, 0.95, 0.45, 1.0),
            border: rgb(0xd0d0d6),
            destructive: rgb(0xdc2626),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
