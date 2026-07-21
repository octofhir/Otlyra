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
enum UserEvent {
    /// A menu item was chosen.
    Menu(MenuId),
    /// Something off the loop's thread asked for a frame.
    Woken,
}

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

/// How long the loop waits before an animated frame. Sixty a second, which is
/// enough for a spinner and cheap enough that nothing else has to opt out.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Open one window, paint it with `painter`, and return when it closes.
///
/// The loop blocks in `ControlFlow::Wait`, so nothing here may request a redraw
/// unconditionally.
pub fn run(config: WindowConfig, painter: &mut dyn Painter) -> Result<(), PlatformError> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|error| PlatformError::EventLoop(error.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    // muda dispatches on its own callback; hand activations to winit so the loop
    // wakes and the app sees one ordered stream of events.
    let proxy = event_loop.create_proxy();
    muda::MenuEvent::set_event_handler(Some(move |event: muda::MenuEvent| {
        if let Some(id) = command_from_muda(&event) {
            let _ = proxy.send_event(UserEvent::Menu(id));
        }
    }));

    // A wake is a message on a channel rather than a proxy handed out directly:
    // the proxy is winit's type and no crate above this one may name it.
    let (wake_sender, wake_receiver) = std::sync::mpsc::channel();
    let wake_proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        // Ends when the last waker is dropped, which is when the browser goes.
        while wake_receiver.recv().is_ok() {
            if wake_proxy.send_event(UserEvent::Woken).is_err() {
                break;
            }
        }
    });
    painter.set_waker(crate::Waker::new(wake_sender));

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

impl ApplicationHandler<UserEvent> for WindowedApp<'_> {
    /// The loop woke because an animated frame is due; ask for it.
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.painter.animating()
            && let Some(window) = self.window.as_ref()
        {
            window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(id) => {
                tracing::debug!(id = ?id, "menu command");
                self.deliver(PlatformEvent::MenuCommand(id));
            }
            UserEvent::Woken => self.deliver(PlatformEvent::Woken),
        }
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
                self.painter.on_event(scroll_event(delta, scale));
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => match self.redraw() {
                Err(error) => self.fail(event_loop, error),
                Ok(Presented::Frame | Presented::Occluded) => {
                    self.dropped_frames = 0;
                    // A painter that says it is animating gets the next frame at
                    // the display's pace; one that does not gets none, and the
                    // loop goes back to blocking.
                    if self.painter.animating() {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(
                            std::time::Instant::now() + FRAME_INTERVAL,
                        ));
                    } else {
                        event_loop.set_control_flow(ControlFlow::Wait);
                    }
                }
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

/// Turn one winit scroll delta into the event the browser above understands.
///
/// Kept apart from the handler so the one thing here that is easy to get
/// backwards can be looked at directly. Two facts decide it, and both are
/// winit's rather than ours:
///
/// - Which variant arrives says which device it was. Winit reports a *precise*
///   delta as pixels and everything else as lines, and precise means a trackpad.
/// - Both variants use the same sign: positive means the content should move
///   down, which is the reader going *up*. Our event says how far down the page
///   the reader went, so it is negated exactly once — here.
///
/// On macOS the natural-scrolling preference has already been applied to the
/// delta by the system, to a wheel and a trackpad alike, so the one negation
/// serves both and neither device needs a case of its own.
fn scroll_event(delta: winit::event::MouseScrollDelta, scale: f64) -> PlatformEvent {
    let (x, y, source) = match delta {
        // A notch, not a distance. What it is worth in pixels is a platform
        // convention; 40 is the figure browsers settled on.
        winit::event::MouseScrollDelta::LineDelta(x, y) => (
            f64::from(x) * LINE_SCROLL,
            f64::from(y) * LINE_SCROLL,
            crate::ScrollSource::Wheel,
        ),
        // Already a distance, in device pixels. Multiplied by nothing: a
        // trackpad has said how far, and a browser that scaled it would move a
        // page by a screen for a gesture that moved a finger a hair.
        winit::event::MouseScrollDelta::PixelDelta(position) => (
            position.x / scale,
            position.y / scale,
            crate::ScrollSource::Trackpad,
        ),
    };
    PlatformEvent::Scroll {
        x: -x,
        y: -y,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;
    use winit::event::MouseScrollDelta;

    #[test]
    fn a_wheel_notch_is_worth_a_notch_and_says_it_was_a_wheel() {
        // Winit's positive is the content moving down, which is the reader going
        // up the page, so the event that comes out is negative.
        let event = scroll_event(MouseScrollDelta::LineDelta(0.0, 1.0), 1.0);
        assert_eq!(
            event,
            PlatformEvent::Scroll {
                x: 0.0,
                y: -LINE_SCROLL,
                source: crate::ScrollSource::Wheel,
            }
        );
    }

    #[test]
    fn a_trackpad_delta_is_a_distance_in_logical_pixels() {
        // Precise deltas arrive in device pixels, so a 2× display reports twice
        // as many for the same gesture.
        let event = scroll_event(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 24.0)),
            2.0,
        );
        assert_eq!(
            event,
            PlatformEvent::Scroll {
                x: 0.0,
                y: -12.0,
                source: crate::ScrollSource::Trackpad,
            }
        );
    }

    #[test]
    fn both_devices_agree_about_which_way_is_down() {
        // The whole point of one negation in one place: a gesture and a notch
        // that winit reports the same way come out of here the same way too.
        let wheel = scroll_event(MouseScrollDelta::LineDelta(0.0, -1.0), 1.0);
        let trackpad = scroll_event(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -1.0)),
            1.0,
        );
        let down = |event| match event {
            PlatformEvent::Scroll { y, .. } => y > 0.0,
            _ => unreachable!("scroll_event only makes scrolls"),
        };
        assert!(down(wheel), "a wheel rolled away from the reader goes down");
        assert!(down(trackpad), "and so does the same gesture on a trackpad");
    }
}
