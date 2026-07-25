//! # otlyra-gfx — the rasterizer seam
//!
//! ## Purpose
//!
//! Owns everything about *how* pixels are produced and nothing about *what* is on
//! the page. It defines [`PaintTarget`], the single seam between display-list
//! construction and rasterization, and ships two implementations of it.
//!
//! The trait exists to make the choice of rasterizer reversible.
//!
//! ## Contents
//!
//! - [`PaintTarget`] — seven required methods, everything else provided.
//! - [`Glyph`] — a positioned glyph in a shaped run's local space.
//! - [`RecordingPainter`] / [`PaintOp`] — a backend that records instead of drawing;
//!   the snapshot-test seam, and the second implementation that keeps the trait
//!   from being shaped like Skia.
//! - [`SkiaPainter`] — the primary backend, over `skia-safe`.
//!
//! ## Invariants
//!
//! 1. **No engine types cross into this crate.** `otlyra-gfx` must never depend on
//!    DOM, CSS, layout, HTML, network or script crates. It speaks only geometry
//!    (`kurbo`), brushes (`peniko`) and fonts.
//! 2. **No Skia or wgpu handle escapes this crate** except through the explicitly
//!    documented presentation types. Nothing outside `otlyra-gfx` holds an
//!    `skia_safe::Surface`.
//! 3. **[`PaintTarget`] is object safe.** Shapes cross the seam as
//!    `&dyn PaintShape`, never `impl Shape`, so `Box<dyn PaintTarget>` works and a
//!    runtime renderer switch stays possible. Asserted at compile time in
//!    `paint_target.rs`.
//! 4. **Adding a required method is a four-backend cost.** New capability goes in as
//!    a provided method over the seven, or it does not go in.

mod display_list;
mod hit_test;
mod paint_target;
mod recording;
mod render;
mod skia;

pub use display_list::{DisplayItem, DisplayList, FontId, FontTable, HitTestId, ImageResource};
pub use hit_test::{Hit, hit_test, hit_test_all};
pub use paint_target::{Glyph, PaintShape, PaintTarget};
pub use peniko::ImageBrushRef;
pub use recording::{PaintOp, RecordingPainter};
pub use render::render;
pub use skia::{SkiaError, SkiaPainter, decode_image};

/// Re-exported so downstream crates speak exactly the geometry types the seam does.
pub use kurbo;
/// Re-exported so downstream crates speak exactly the brush types the seam does.
pub use peniko;
