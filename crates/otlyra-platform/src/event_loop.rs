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

use crate::a11y::Accessibility;
use crate::menu::{NativeMenu, command_from_muda};
use crate::present::{Presented, Presenter};
use crate::{MenuId, Painter, PlatformEvent, Viewport, WindowConfig};

/// Logical pixels one wheel notch scrolls.
const LINE_SCROLL: f64 = 40.0;

/// How many frames in a row the swapchain may refuse before we stop asking.
const MAX_DROPPED_FRAMES: u32 = 8;

/// Translate a winit key into our own vocabulary. `None` for keys nothing acts on.
fn translate_key(key: &winit::keyboard::Key) -> Option<crate::Key> {
    use winit::keyboard::{Key as WinitKey, NamedKey};

    Some(match key {
        WinitKey::Named(named) => match named {
            NamedKey::Enter => crate::Key::Enter,
            NamedKey::Backspace => crate::Key::Backspace,
            NamedKey::Delete => crate::Key::Delete,
            NamedKey::Escape => crate::Key::Escape,
            NamedKey::Tab => crate::Key::Tab,
            NamedKey::ArrowLeft => crate::Key::Left,
            NamedKey::ArrowRight => crate::Key::Right,
            NamedKey::ArrowUp => crate::Key::Up,
            NamedKey::ArrowDown => crate::Key::Down,
            NamedKey::Home => crate::Key::Home,
            NamedKey::End => crate::Key::End,
            NamedKey::PageUp => crate::Key::PageUp,
            NamedKey::PageDown => crate::Key::PageDown,
            NamedKey::F5 => crate::Key::F5,
            _ => return None,
        },
        WinitKey::Character(text) => crate::Key::Character(text.chars().next()?),
        _ => return None,
    })
}

/// Menu activations arrive on muda's own callback, off winit's event path, so they
/// are forwarded through the event loop proxy. Without this the loop would sit in
/// `Wait` and the menu would appear to do nothing until the next mouse move.
#[derive(Debug)]
struct MenuActivated(MenuId);

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
    /// The menu bar could not be built.
    #[error("menu bar failed: {0}")]
    Menu(#[source] Box<dyn std::error::Error + Send + Sync>),
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

impl From<crate::menu::MenuError> for PlatformError {
    fn from(error: crate::menu::MenuError) -> Self {
        Self::Menu(Box::new(error))
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
    let event_loop = EventLoop::<MenuActivated>::with_user_event()
        .build()
        .map_err(|error| PlatformError::EventLoop(error.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    // muda dispatches on its own callback; hand activations to winit so the loop
    // wakes and the app sees one ordered stream of events.
    let proxy = event_loop.create_proxy();
    muda::MenuEvent::set_event_handler(Some(move |event: muda::MenuEvent| {
        if let Some(id) = command_from_muda(&event) {
            let _ = proxy.send_event(MenuActivated(id));
        }
    }));

    let mut app = WindowedApp {
        config,
        painter,
        window: None,
        presenter: None,
        rasterizer: None,
        menu: None,
        a11y: None,
        frames: 0,
        modifiers: crate::Modifiers::default(),
        cursor: crate::Cursor::default(),
        dropped_frames: 0,
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
    /// Held for the application's lifetime: dropping it removes the menu bar.
    menu: Option<NativeMenu>,
    /// The accessibility adapter. Absent if it could not be created, which is a
    /// degraded browser and not a broken one.
    a11y: Option<Accessibility>,
    frames: u64,
    /// Modifier state, tracked here because winit reports it as its own event and
    /// every key press needs it.
    modifiers: crate::Modifiers,
    /// The cursor currently set, so it is only changed when it actually changes.
    cursor: crate::Cursor,
    /// Consecutive frames the swapchain refused, so retrying stays bounded.
    dropped_frames: u32,
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

    /// Hand an event up and ask for a frame.
    ///
    /// Every input event may change what is on screen, and the loop blocks in
    /// `Wait`: an event nobody follows with a redraw request is an event the user
    /// sees no result from.
    fn deliver(&mut self, event: PlatformEvent) {
        self.painter.on_event(event);

        let cursor = self.painter.cursor();
        if cursor != self.cursor
            && let Some(window) = self.window.as_ref()
        {
            window.set_cursor(match cursor {
                crate::Cursor::Default => winit::window::CursorIcon::Default,
                crate::Cursor::Pointer => winit::window::CursorIcon::Pointer,
                crate::Cursor::Text => winit::window::CursorIcon::Text,
            });
            self.cursor = cursor;
        }

        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn redraw(&mut self) -> Result<Presented, PlatformError> {
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
        let Some(presenter) = self.presenter.as_mut() else {
            return Ok(Presented::Dropped);
        };
        presenter.resize(viewport);
        let outcome = presenter.present(&pixels, viewport.width, viewport.height)?;

        if outcome == Presented::Frame {
            self.frames += 1;
            tracing::debug!(frame = self.frames, "frame presented");
        }

        // After the frame, because the tree describes what is now on screen.
        if let Some(update) = self.painter.accessibility()
            && let Some(a11y) = self.a11y.as_mut()
        {
            a11y.update(update);
        }

        Ok(outcome)
    }
}

impl ApplicationHandler<MenuActivated> for WindowedApp<'_> {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: MenuActivated) {
        tracing::debug!(id = ?event.0, "menu command");
        self.painter.on_event(PlatformEvent::MenuCommand(event.0));
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // macOS can resume an already-running application; recreating the
            // window here would orphan the surface.
            return;
        }

        // Both before the window, so the icon and menu bar are in place the moment
        // anything is on screen.
        if let Some(icon) = self.config.icon {
            crate::icon::set_dock_icon(icon);
        }

        if !self.config.menu_bar.menus.is_empty() {
            match NativeMenu::install(&self.config.menu_bar) {
                Ok(menu) => self.menu = Some(menu),
                Err(error) => {
                    self.fail(event_loop, error.into());
                    return;
                }
            }
        }

        // Created hidden: the accessibility adapter must exist before the window is
        // shown for the first time, and it says so by panicking otherwise.
        let attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_visible(false)
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

        self.a11y = Some(Accessibility::new(event_loop, &window));
        window.set_visible(true);

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
        if let (Some(a11y), Some(window)) = (self.a11y.as_mut(), self.window.as_ref()) {
            a11y.process_event(window, &event);
        }

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
            // The window became visible or came to the front. Both mean the last
            // frame may never have reached the screen — a swapchain hands back an
            // occluded texture and the paint goes nowhere — and in a loop that
            // blocks in `Wait` nothing else will ask for another one.
            WindowEvent::Occluded(false) | WindowEvent::Focused(true) => {
                self.dropped_frames = 0;
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                self.modifiers = crate::Modifiers {
                    shift: state.shift_key(),
                    control: state.control_key(),
                    alt: state.alt_key(),
                    command: state.super_key(),
                };
            }

            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.viewport().scale_factor;
                self.deliver(PlatformEvent::PointerMoved {
                    x: position.x / scale,
                    y: position.y / scale,
                });
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == winit::event::MouseButton::Left {
                    self.deliver(match state {
                        winit::event::ElementState::Pressed => PlatformEvent::PointerPressed,
                        winit::event::ElementState::Released => PlatformEvent::PointerReleased,
                    });
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != winit::event::ElementState::Pressed {
                    return;
                }
                if let Some(key) = translate_key(&event.logical_key) {
                    self.deliver(PlatformEvent::KeyPressed {
                        key,
                        modifiers: self.modifiers,
                    });
                }
                // Text is what the key produced *after* layout, dead keys and the
                // input method had their say — which is why it is a separate event
                // and not something this layer works out from the key.
                if !self.modifiers.command && !self.modifiers.control {
                    for character in event.text.iter().flat_map(|text| text.chars()) {
                        if !character.is_control() {
                            self.deliver(PlatformEvent::TextInput(character));
                        }
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let scale = self.viewport().scale_factor;
                let (x, y) = match delta {
                    // A notch, not a distance. What it is worth in pixels is a
                    // platform convention; 40 is the figure browsers settled on.
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        (f64::from(x) * LINE_SCROLL, f64::from(y) * LINE_SCROLL)
                    }
                    // Already a distance, in device pixels.
                    winit::event::MouseScrollDelta::PixelDelta(position) => {
                        (position.x / scale, position.y / scale)
                    }
                };
                // Negated: winit reports which way the wheel turned, and the event
                // says which way the content should move.
                self.painter
                    .on_event(PlatformEvent::Scroll { x: -x, y: -y });
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => match self.redraw() {
                Err(error) => self.fail(event_loop, error),
                Ok(Presented::Frame | Presented::Occluded) => self.dropped_frames = 0,
                // Ask for the frame again. Bounded, because a swapchain that fails
                // forever must not turn the blocking loop into a spinning one — that
                // would trade a black window for a hot CPU.
                Ok(Presented::Dropped) => {
                    self.dropped_frames += 1;
                    if self.dropped_frames <= MAX_DROPPED_FRAMES {
                        if let Some(window) = self.window.as_ref() {
                            window.request_redraw();
                        }
                    } else {
                        tracing::warn!(
                            dropped = self.dropped_frames,
                            "the swapchain keeps refusing frames; waiting for the next event"
                        );
                    }
                }
            },
            _ => {}
        }
    }
}
