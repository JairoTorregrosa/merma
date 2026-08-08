//! Presentation-only styling shim for the text surfaces (brief / doctor).
//!
//! ANSI escapes are emitted only when stdout is a real terminal AND `NO_COLOR`
//! is unset — piped output stays byte-plain, so scripts and tests see exactly
//! the template text. All width/padding math must run on plain strings BEFORE
//! painting; never measure a painted string. `--json` branches must never call
//! into this module.
//!
//! v0.1's teal accent was graphic ink only; v0.2 deleted every graphic
//! surface, so teal is retired by decision (DESIGN.md), not omission. If a
//! graphic surface ever returns, teal returns with it.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// True when stdout may carry ANSI styling.
pub fn styled() -> bool {
    static STYLED: OnceLock<bool> = OnceLock::new();
    *STYLED
        .get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn paint(code: &str, s: &str) -> String {
    if styled() && !s.is_empty() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Bold: the few values that answer the surface's question.
pub fn bold(s: &str) -> String {
    paint("1", s)
}

/// Secondary text: labels, captions, units, separators. ANSI DarkGray (90) —
/// themed by the terminal, readable on dark and light backgrounds alike.
pub fn dim(s: &str) -> String {
    paint("90", s)
}

/// Yellow — reserved for the `⚠` glyph.
pub fn warn(s: &str) -> String {
    paint("33", s)
}

/// Green `✓` — true status only, never decoration.
pub fn ok_glyph() -> String {
    paint("32", "✓")
}

/// Yellow `⚠`.
pub fn warn_glyph() -> String {
    warn("⚠")
}

/// Red `✗`.
pub fn err_glyph() -> String {
    paint("31", "✗")
}
