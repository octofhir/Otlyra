//! # otlyra-platform — the OS boundary
//!
//! ## Purpose
//!
//! Owns the native window, the GPU surface and the event loop, and translates all
//! three into vocabulary the rest of the browser can use. Everything that knows
//! what an operating system is lives here.
//!
//! ## Contents
//!
//! - [`run`] — opens one window and drives it to close. This is the event loop.
//! - [`Painter`] — what the embedder implements to put something on screen. It is
//!   handed a [`otlyra_gfx::PaintTarget`] and a [`Viewport`], and nothing else.
//! - [`PlatformEvent`], [`Viewport`], [`WindowConfig`] — the translated vocabulary.
//! - [`render_offscreen`] — the same paint call with no window at all, which is
//!   what `--screenshot` and the image tests use.
//!
//! ## Invariants
//!
//! 1. **No `winit::` type appears in this crate's public API.** Not in a signature,
//!    not in a re-export, not in a public field, not behind a type alias. winit
//!    0.31 deletes the `Event` enum, makes `Window` a trait, renames `inner_size`
//!    to `surface_size` and deprecates the IME API; this rule is what confines that
//!    migration to this crate. `tests/public_api.rs` enforces it.
//! 2. **No `wgpu::` type appears in this crate's public API** either, for the same
//!    reason at a faster cadence — wgpu ships a breaking major every twelve weeks.
//! 3. **The loop blocks.** `ControlFlow::Wait`, never `Poll`. Idle CPU under 1% is
//!    a requirement, not an optimization: a change that spins is a regression even
//!    if it renders correctly.
//! 4. **This crate never sees a DOM, style, layout or script type.** It depends on
//!    `otlyra-gfx` and nothing else of ours.
//! 5. **All sizes crossing this boundary are device pixels**, with the scale factor
//!    reported alongside. Logical pixels are the engine's business, not the
//!    platform's.

mod event_loop;
mod present;

pub use event_loop::{PlatformError, run};

use otlyra_gfx::PaintTarget;

/// The drawable area, in device pixels, plus the factor that produced it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Viewport {
    /// Drawable width in device (physical) pixels.
    pub width: u32,
    /// Drawable height in device (physical) pixels.
    pub height: u32,
    /// Device pixels per logical pixel. 2.0 on a typical Retina display.
    pub scale_factor: f64,
}

impl Viewport {
    /// Construct a viewport, clamping both dimensions to at least one pixel.
    ///
    /// A minimized window is a real state everywhere, and a zero-area surface is an
    /// allocation failure in both wgpu and Skia. Clamping here means one place in
    /// the codebase has to know that.
    pub fn new(width: u32, height: u32, scale_factor: f64) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            scale_factor,
        }
    }

    /// Width in logical pixels.
    pub fn logical_width(&self) -> f64 {
        f64::from(self.width) / self.scale_factor
    }

    /// Height in logical pixels.
    pub fn logical_height(&self) -> f64 {
        f64::from(self.height) / self.scale_factor
    }
}

/// Something happened that the browser above may care about.
///
/// Deliberately small: a variant with no consumer only makes the translation layer
/// lie about what it handles.
#[derive(Copy, Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PlatformEvent {
    /// The window became visible and has a surface. Sent exactly once per window
    /// before any paint.
    SurfaceReady(Viewport),
    /// The drawable changed size, the scale factor changed, or both.
    Resized(Viewport),
    /// The user asked to close the window. The loop exits after this is delivered.
    CloseRequested,
}

/// The embedder's side of the boundary: given a target and a viewport, draw.
pub trait Painter {
    /// React to a platform event. Default: ignore it.
    fn on_event(&mut self, event: PlatformEvent) {
        let _ = event;
    }

    /// Paint one frame. `target` has already been reset for this frame.
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport);
}

/// How the window should be created.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    /// Title bar text.
    pub title: String,
    /// Initial size in *logical* pixels — this is the one place logical units are
    /// the natural unit, because that is what the user and the OS agree on.
    pub logical_size: (f64, f64),
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "Otlyra".to_owned(),
            logical_size: (1024.0, 768.0),
        }
    }
}

/// Paint one frame with no window, no GPU and no event loop, and return the PNG.
///
/// Free of both winit and wgpu on purpose: CI has no display server, and a test
/// that needs a compositor is a test that gets disabled within a month.
pub fn render_offscreen(
    painter: &mut dyn Painter,
    viewport: Viewport,
) -> Result<Vec<u8>, PlatformError> {
    let span = tracing::info_span!("paint", width = viewport.width, height = viewport.height);
    let mut target =
        otlyra_gfx::SkiaPainter::new_raster(viewport.width, viewport.height).map_err(Box::new)?;

    {
        let _guard = span.enter();
        painter.on_event(PlatformEvent::SurfaceReady(viewport));
        target.reset();
        painter.paint(&mut target, viewport);
    }

    let _present = tracing::info_span!("present", mode = "offscreen").entered();
    Ok(target.encode_png().map_err(Box::new)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_sized_viewports_are_clamped_not_propagated() {
        let viewport = Viewport::new(0, 0, 2.0);
        assert_eq!(viewport.width, 1);
        assert_eq!(viewport.height, 1);
    }

    #[test]
    fn logical_dimensions_divide_by_the_scale_factor() {
        let viewport = Viewport::new(2048, 1536, 2.0);
        assert_eq!(viewport.logical_width(), 1024.0);
        assert_eq!(viewport.logical_height(), 768.0);
    }
}
