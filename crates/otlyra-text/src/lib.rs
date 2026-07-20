//! # otlyra-text — fonts, shaping and measurement
//!
//! ## Purpose
//!
//! Turns a string plus a font specification into positioned glyphs. It owns font
//! enumeration, CSS family matching, fallback, shaping and line breaking, and it is
//! the only crate that knows what a font file is.
//!
//! It does not rasterize. The glyphs it produces are handed to a
//! [`otlyra_gfx::PaintTarget`], which decides what they look like.
//!
//! ## Contents
//!
//! - [`FontStack`] — a CSS font stack: named families then a generic fallback.
//! - [`TextEngine`] — owns the font collection and the shaping caches.
//! - [`ShapedRun`] — one run of glyphs in one font at one size, positioned.
//! - [`TEST_FAMILY`] — a repo-vendored family, registered unconditionally.
//!
//! ## Invariants
//!
//! 1. **Line breaking follows web behaviour, not bare UAX#14.** parley's
//!    web-compatible override table is applied to every layout, without exception.
//!    UAX#14 alone disagrees with what shipping browsers do at several ASCII pairs,
//!    and breaking a line where the web would not is a compatibility bug that shows
//!    up as visibly wrong text wrapping.
//! 2. **Measurement tests use the vendored font, never a system font.** System
//!    fonts differ per machine and per OS version, so a golden number measured
//!    against one is a golden number that fails on someone else's laptop.
//! 3. **Positions are in logical pixels**, relative to the layout origin. Applying
//!    the device scale is the caller's job, as it is everywhere else.
//! 4. **This crate never sees a DOM, style or layout type.** Its input is a string
//!    and a font specification.

mod engine;
mod stack;

pub use engine::{
    Brush, Decoration, LineMetrics, PlacedSpacer, ShapedRun, ShapedText, Spacer, TextEngine,
    TextMetrics, TextSpan,
};
pub use stack::{Family, FontStack, GenericFamily};

/// Re-exported so callers name the same font handle type the shaper does.
pub use parley::FontData;

/// The family name of the font vendored into this crate.
///
/// It is registered on every [`TextEngine`], not just in tests: a golden image
/// rendered against a system font is a golden image that only holds on the machine
/// that produced it.
pub const TEST_FAMILY: &str = "Otlyra Test";

/// The vendored font itself. Roboto Regular, Apache-2.0, the same licence as this
/// repository. `fonts/LICENSE.txt` carries the text.
pub const TEST_FONT: &[u8] = include_bytes!("../fonts/Roboto-Regular.ttf");
