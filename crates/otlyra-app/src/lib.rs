//! # otlyra-app — the browser shell
//!
//! ## Purpose
//!
//! The top of the stack. It owns the event loop, and will own navigation, the
//! document lifecycle and the session. Every other crate is a library it drives;
//! nothing depends on this one.
//!
//! ## Contents
//!
//! - [`menu`] — the native menu bar and the commands it invokes.
//! - [`observability`] — tracing setup and the fixed span vocabulary.
//! - [`a11y`] — the accessibility tree, built from the document.
//! - [`browser`] — tabs, navigation, and the loop's `Painter`.
//! - [`page`] — one document: box tree, layout, scroll position.
//! - [`ui`] — the tab strip and address bar, drawn with our own stack.
//! - [`scene`] — the placeholder scene, replaced once a display list exists.
//! - [`run_window`] / [`write_screenshot`] — the two entry points.
//!
//! ## Invariants
//!
//! 1. **Span names are fixed** in [`observability::spans`] and never renamed; every
//!    performance target is stated in terms of them.
//! 2. **Windowed and screenshot rendering share one [`otlyra_platform::Painter`].**
//!    A screenshot that can drift from what the window shows is worthless as a test.
//! 3. **This crate holds no GPU or rasterizer handle.** It hands a `Painter` to
//!    `otlyra-platform` and gets pixels or a PNG back.

/// The application icon, shown in the Dock.
///
/// Carried as encoded bytes rather than a path because a `cargo run` binary has no
/// bundle to load resources from.
///
/// The thousand-and-twenty-four source, which carries the margin the platform's
/// own icon grid leaves — the artwork is eight hundred and twenty-four of it. An
/// icon drawn edge to edge is laid out at the same box as every other one and so
/// comes out a quarter larger than its neighbours in the Dock.
pub const ICON: &[u8] = include_bytes!("../../../assets/logo/icon-1024.png");

/// The mark, drawn on an empty tab.
///
/// Encoded bytes for the same reason the icon is: a `cargo run` binary has no
/// bundle to load resources from.
pub const MARK: &[u8] = include_bytes!("../../../assets/logo/mark-256.png");

pub mod a11y;
pub mod about;
pub mod bidi;
pub mod browser;
pub mod clipboard;
pub mod fetcher;
pub mod history;
pub mod inspector;
pub mod mcp;
pub mod menu;
pub mod observability;
pub mod page;
pub mod preferences;
pub mod scene;
pub mod settings;
pub mod ui;
pub mod widget;

use std::path::Path;

use otlyra_platform::{Painter, Viewport, WindowConfig};

/// Failures that reach `main`.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The platform layer failed.
    #[error(transparent)]
    Platform(#[from] otlyra_platform::PlatformError),
    /// Writing the screenshot failed.
    #[error("failed to write screenshot to {path}: {source}")]
    ScreenshotWrite {
        /// The path we tried to write.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Open a window and run until it closes.
pub fn run_window(config: WindowConfig, painter: &mut dyn Painter) -> Result<(), AppError> {
    Ok(otlyra_platform::run(config, painter)?)
}

/// Render exactly one frame at `viewport` and write it to `path` as a PNG.
///
/// Touches neither winit nor wgpu, so it runs on a CI machine with no display
/// server. That is what lets the image tests be a merge gate.
pub fn write_screenshot(
    painter: &mut dyn Painter,
    viewport: Viewport,
    path: &Path,
) -> Result<(), AppError> {
    let png = otlyra_platform::render_offscreen(painter, viewport)?;
    std::fs::write(path, &png).map_err(|source| AppError::ScreenshotWrite {
        path: path.display().to_string(),
        source,
    })?;
    tracing::info!(path = %path.display(), bytes = png.len(), "screenshot written");
    Ok(())
}
