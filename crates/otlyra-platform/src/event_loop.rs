//! The winit event loop, and the wall that keeps winit inside this file.
//!
//! Every `winit::` reference in this crate is in this module. When 0.31 lands, the
//! diff is bounded by it.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use otlyra_gfx::{PaintTarget, SkiaPainter};

use crate::present::Presenter;
use crate::{Painter, PlatformEvent, Viewport, WindowConfig};

/// Anything that can go wrong opening or driving a window.
///
/// Opaque about its causes on purpose: naming `wgpu::RequestDeviceError` or
/// `winit::error::EventLoopError` here would put a twelve-week-cadence type in the
/// public API. The source chain survives; the concrete types are not named.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The OS event loop could not be created or ran into a fatal error.
    #[error("event loop failed: {0}")]
    EventLoop(String),
    /// The window itself could not be created.
    #[error("window creation failed: {0}")]
    WindowCreation(String),
    /// The GPU presentation path failed.
    #[error("gpu presentation failed: {0}")]
    Gpu(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The rasterizer failed to allocate or read back a surface.
    #[error("rasterizer failed: {0}")]
    Rasterizer(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl From<Box<otlyra_gfx::SkiaError>> for PlatformError {
    fn from(error: Box<otlyra_gfx::SkiaError>) -> Self {
        Self::Rasterizer(error)
    }
}

impl From<crate::present::PresentError> for PlatformError {
    fn from(error: crate::present::PresentError) -> Self {
        Self::Gpu(Box::new(error))
    }
}

/// Open one window, paint it with `painter`, and return when it closes.
///
/// The loop blocks in `ControlFlow::Wait`, so nothing here may request a redraw
/// unconditionally.
pub fn run(config: WindowConfig, painter: &mut dyn Painter) -> Result<(), PlatformError> {
    let event_loop =
        EventLoop::new().map_err(|error| PlatformError::EventLoop(error.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = WindowedApp {
        config,
        painter,
        window: None,
        presenter: None,
        rasterizer: None,
        frames: 0,
        failure: None,
    };

    event_loop
        .run_app(&mut app)
        .map_err(|error| PlatformError::EventLoop(error.to_string()))?;

    match app.failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct WindowedApp<'p> {
    config: WindowConfig,
    painter: &'p mut dyn Painter,
    window: Option<Arc<Window>>,
    presenter: Option<Presenter>,
    rasterizer: Option<SkiaPainter>,
    frames: u64,
    /// First fatal error, so `run` can return it once the loop unwinds. An
    /// `ApplicationHandler` callback cannot return one, and panicking across the OS
    /// callback boundary is worse.
    failure: Option<PlatformError>,
}

impl WindowedApp<'_> {
    fn viewport(&self) -> Viewport {
        let Some(window) = self.window.as_ref() else {
            return Viewport::new(1, 1, 1.0);
        };
        let size = window.inner_size();
        Viewport::new(size.width, size.height, window.scale_factor())
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: PlatformError) {
        tracing::error!(%error, "fatal platform error; closing the window");
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        event_loop.exit();
    }

    fn redraw(&mut self) -> Result<(), PlatformError> {
        let viewport = self.viewport();

        let rasterizer = match self.rasterizer.as_mut() {
            Some(rasterizer) => {
                rasterizer
                    .resize(viewport.width, viewport.height)
                    .map_err(Box::new)?;
                rasterizer
            }
            None => {
                let new =
                    SkiaPainter::new_raster(viewport.width, viewport.height).map_err(Box::new)?;
                self.rasterizer.insert(new)
            }
        };

        {
            let _paint = tracing::info_span!(
                "paint",
                width = viewport.width,
                height = viewport.height,
                scale_factor = viewport.scale_factor
            )
            .entered();
            rasterizer.reset();
            self.painter.paint(rasterizer, viewport);
        }

        let _present = tracing::info_span!("present", mode = "blit").entered();
        let pixels = rasterizer.read_rgba8().map_err(Box::new)?;
        if let Some(presenter) = self.presenter.as_mut() {
            presenter.resize(viewport);
            presenter.present(&pixels, viewport.width, viewport.height)?;
        }
        self.frames += 1;
        tracing::debug!(frame = self.frames, "frame presented");
        Ok(())
    }
}

impl ApplicationHandler for WindowedApp<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // macOS can resume an already-running application; recreating the
            // window here would orphan the surface.
            return;
        }

        let attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.logical_size.0,
                self.config.logical_size.1,
            ));

        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(event_loop, PlatformError::WindowCreation(error.to_string()));
                return;
            }
        };

        let viewport = {
            let size = window.inner_size();
            Viewport::new(size.width, size.height, window.scale_factor())
        };

        match Presenter::new(Arc::clone(&window), viewport) {
            Ok(presenter) => self.presenter = Some(presenter),
            Err(error) => {
                self.fail(event_loop, error.into());
                return;
            }
        }

        window.request_redraw();
        self.window = Some(window);
        self.painter.on_event(PlatformEvent::SurfaceReady(viewport));
        tracing::info!(
            width = viewport.width,
            height = viewport.height,
            scale_factor = viewport.scale_factor,
            "window ready"
        );
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.painter.on_event(PlatformEvent::CloseRequested);
                event_loop.exit();
            }
            // Both mean the same thing above us: the drawable changed. winit
            // separates them because scale and pixel size can change alone.
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                let viewport = self.viewport();
                self.painter.on_event(PlatformEvent::Resized(viewport));
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw() {
                    self.fail(event_loop, error);
                }
            }
            _ => {}
        }
    }
}
