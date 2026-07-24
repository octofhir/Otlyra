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
use crate::{FrameRequest, MenuId, Painter, PlatformEvent, Viewport, WindowConfig};

/// Logical pixels one wheel notch scrolls.
const LINE_SCROLL: f64 = 40.0;

/// How many frames in a row the swapchain may refuse before we stop asking.
const MAX_DROPPED_FRAMES: u32 = 8;

/// Translate winit's theme into our own vocabulary.
fn translate_theme(theme: winit::window::Theme) -> crate::ColorScheme {
    match theme {
        winit::window::Theme::Light => crate::ColorScheme::Light,
        winit::window::Theme::Dark => crate::ColorScheme::Dark,
    }
}

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
enum UserEvent {
    /// A menu item was chosen.
    Menu(MenuId),
    /// Something off the loop's thread asked for a frame.
    Woken,
    /// Backend discovery completed; the main thread may attach the window.
    PresenterInstanceReady(Box<crate::present::PresenterInstance>),
    /// GPU initialization completed without blocking the window event loop.
    PresenterReady(Box<Result<Presenter, crate::present::PresentError>>),
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
    /// The requested machine-readable startup report could not be written.
    #[error("startup report failed: {0}")]
    StartupReport(#[source] std::io::Error),
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

/// Work performed by the platform frame path since the window opened.
///
/// Cumulative counters make a no-op visible: tests and profiles can compare two
/// snapshots and require every field to stay put.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct WorkCounters {
    events_delivered: u64,
    redraw_requests: u64,
    frames_started: u64,
    frames_presented: u64,
    accessibility_updates: u64,
    rasterized_pixels: u64,
    uploaded_bytes: u64,
}

/// Coalesces immediate frames and owns the one future wake-up.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct FrameScheduler {
    redraw_pending: bool,
    deadline: Option<std::time::Instant>,
}

impl FrameScheduler {
    /// Record one request. `true` means winit needs a new redraw request now.
    fn request(&mut self, request: FrameRequest, now: std::time::Instant) -> bool {
        match request {
            FrameRequest::None => false,
            FrameRequest::Now => {
                // This frame observes everything that a previously scheduled
                // animation frame would have observed. The painter will publish
                // its next deadline after presentation if the animation continues.
                self.deadline = None;
                self.request_now()
            }
            FrameRequest::Vsync => {
                self.set_deadline(now + FRAME_INTERVAL);
                false
            }
            FrameRequest::At(deadline) if deadline <= now => self.request_now(),
            FrameRequest::At(deadline) => {
                self.set_deadline(deadline);
                false
            }
        }
    }

    fn request_now(&mut self) -> bool {
        if self.redraw_pending {
            return false;
        }
        self.redraw_pending = true;
        true
    }

    fn set_deadline(&mut self, deadline: std::time::Instant) {
        if self.deadline.is_none_or(|current| deadline < current) {
            self.deadline = Some(deadline);
        }
    }

    /// Turn a reached deadline into one coalesced redraw request.
    fn wake_due(&mut self, now: std::time::Instant) -> bool {
        if self.deadline.is_none_or(|deadline| deadline > now) {
            return false;
        }
        self.deadline = None;
        self.request_now()
    }

    fn redraw_started(&mut self) {
        self.redraw_pending = false;
        self.deadline = None;
    }
}

/// Open one window, paint it with `painter`, and return when it closes.
///
/// The loop blocks in `ControlFlow::Wait`, so nothing here may request a redraw
/// unconditionally.
pub fn run(config: WindowConfig, painter: &mut dyn Painter) -> Result<(), PlatformError> {
    let startup_origin = config.startup_origin;
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
    let presenter_proxy = event_loop.create_proxy();

    let mut app = WindowedApp {
        config,
        painter,
        window: None,
        presenter: None,
        presenter_proxy,
        rasterizer: None,
        menu: None,
        a11y: None,
        frames: 0,
        startup_origin,
        window_visible: None,
        first_frame: None,
        startup_report_written: false,
        prev_layers: Vec::new(),
        composited_once: false,
        scheduler: FrameScheduler::default(),
        work: WorkCounters::default(),
        modifiers: crate::Modifiers::default(),
        pointer: (-1.0, -1.0),
        last_press: None,
        cursor: crate::Cursor::default(),
        pinned_scheme: None,
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
    /// Wakes the UI thread when background GPU initialization completes.
    presenter_proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    rasterizer: Option<SkiaPainter>,
    /// Held for the application's lifetime: dropping it removes the menu bar.
    menu: Option<NativeMenu>,
    /// The accessibility adapter. Absent if it could not be created, which is a
    /// degraded browser and not a broken one.
    a11y: Option<Accessibility>,
    frames: u64,
    /// The executable's startup origin and the milestones reached from it.
    startup_origin: std::time::Instant,
    window_visible: Option<std::time::Instant>,
    first_frame: Option<std::time::Instant>,
    /// Whether benchmark mode has captured its one required presented frame.
    startup_report_written: bool,
    /// Last composited frame's layers — identity, epoch, and rect, in order — so
    /// the next frame's damage is the union of the layers that moved.
    prev_layers: Vec<(crate::LayerId, u64, crate::LayerRect)>,
    /// Whether at least one full composite has run. Until it has, and after any
    /// surface reallocation, the compositor owes a whole-surface frame.
    composited_once: bool,
    /// The only owner of redraw requests and animation deadlines.
    scheduler: FrameScheduler,
    /// Cumulative work, logged with every presented frame.
    work: WorkCounters,
    /// Modifier state, tracked here because winit reports it as its own event and
    /// every key press needs it.
    modifiers: crate::Modifiers,
    /// Where the pointer was last seen, in logical pixels. Kept for the
    /// multi-click radius: winit reports a press with no position on it.
    pointer: (f64, f64),
    /// The last press: when, where, and how many clicks it was the latest of.
    /// What turns three presses into a triple-click.
    last_press: Option<(std::time::Instant, (f64, f64), u32)>,
    /// The cursor currently set, so it is only changed when it actually changes.
    cursor: crate::Cursor,
    /// The scheme the window is currently pinned to, `None` while it follows
    /// the system. Kept so the window is only told when the answer changes —
    /// and so a `ThemeChanged` that merely echoes the pin is not reported as
    /// the system changing its mind.
    pinned_scheme: Option<crate::ColorScheme>,
    /// Consecutive frames the swapchain refused, so retrying stays bounded.
    dropped_frames: u32,
    /// First fatal error, so `run` can return it once the loop unwinds. An
    /// `ApplicationHandler` callback cannot return one, and panicking across the OS
    /// callback boundary is worse.
    failure: Option<PlatformError>,
}

impl WindowedApp<'_> {
    /// How many clicks the press that just happened is the latest of.
    ///
    /// A press within half a second and four logical pixels of the last one
    /// continues its run; anything further in time or place starts a new one.
    /// winit does not count clicks for us, so the counting lives here.
    fn count_click(&mut self) -> u32 {
        const INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        const RADIUS: f64 = 4.0;

        let now = std::time::Instant::now();
        let clicks = match self.last_press {
            Some((when, (x, y), count))
                if now.duration_since(when) <= INTERVAL
                    && (self.pointer.0 - x).abs() <= RADIUS
                    && (self.pointer.1 - y).abs() <= RADIUS =>
            {
                count + 1
            }
            _ => 1,
        };
        self.last_press = Some((now, self.pointer, clicks));
        clicks
    }

    /// Record one startup milestone if the config asked for a trace.
    ///
    /// Takes `&self`: the marks land between ordinary mutations of the loop's
    /// state, and a milestone is never a reason to hold the loop mutably.
    fn mark(&self, name: &'static str) {
        if let Some(trace) = self.config.startup_trace.as_ref() {
            trace.mark(name);
        }
    }

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

    /// Hand an event up and schedule only the frame it asks for.
    fn deliver(&mut self, event: PlatformEvent) {
        self.work.events_delivered += 1;
        tracing::trace!(?event, "platform event delivered");
        let request = self.painter.handle_event(event);

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

        self.sync_window_appearance();
        self.request_frame(request);
    }

    /// The one place that calls winit's `request_redraw`.
    fn request_frame(&mut self, request: FrameRequest) {
        tracing::trace!(?request, "frame requested");
        if !self.scheduler.request(request, std::time::Instant::now()) {
            return;
        }
        self.issue_redraw();
    }

    fn issue_redraw(&mut self) {
        // State may change while the GPU device is being requested. The first
        // frame after PresenterReady observes all of it; rasterizing before
        // there is somewhere to present would only burn startup time.
        if self.presenter.is_none() {
            self.scheduler.redraw_pending = false;
            return;
        }
        let Some(window) = self.window.as_ref() else {
            self.scheduler.redraw_pending = false;
            return;
        };
        window.request_redraw();
        self.work.redraw_requests += 1;
    }

    fn sync_control_flow(&self, event_loop: &ActiveEventLoop) {
        match self.scheduler.deadline {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    /// Pin the window — its titlebar — to what the painter wants, or hand it
    /// back to the system.
    ///
    /// On the way back, the system's current answer is delivered explicitly:
    /// while the window was pinned the system may have changed its mind, and
    /// the painter has to hear about it *now* rather than at the next change.
    fn sync_window_appearance(&mut self) {
        let wanted = self.painter.window_appearance();
        if wanted == self.pinned_scheme {
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        window.set_theme(wanted.map(|scheme| match scheme {
            crate::ColorScheme::Light => winit::window::Theme::Light,
            crate::ColorScheme::Dark => winit::window::Theme::Dark,
        }));
        self.pinned_scheme = wanted;
        if wanted.is_none()
            && let Some(scheme) = self
                .window
                .as_ref()
                .and_then(|w| w.theme())
                .map(translate_theme)
        {
            let request = self
                .painter
                .handle_event(PlatformEvent::AppearanceChanged(scheme));
            self.request_frame(request);
        }
    }

    fn redraw(&mut self) -> Result<Presented, PlatformError> {
        self.scheduler.redraw_started();
        // Only the first frame carries startup marks. Cloning the handle (an `Arc`)
        // once here keeps the marks free of a `&self` borrow that would collide
        // with the rasterizer's `&mut`, and skips the work on every later frame.
        let trace = if self.first_frame.is_none() {
            self.config.startup_trace.clone()
        } else {
            None
        };
        let viewport = self.viewport();

        // Retained-layer path: a painter that publishes a scene re-rasterizes and
        // re-uploads only what moved. Everything else is drawn whole below.
        if let Some(scene) = self.painter.compose(viewport) {
            return self.composite(scene, viewport, &trace);
        }

        self.work.frames_started += 1;
        self.work.rasterized_pixels = self
            .work
            .rasterized_pixels
            .saturating_add(u64::from(viewport.width) * u64::from(viewport.height));

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
        // Chrome build, text shaping, and CPU raster all happen inside `paint`,
        // which interleaves list-building and Skia draw calls; the seam here can
        // separate them from readback but not from one another.
        if let Some(trace) = trace.as_ref() {
            trace.mark("chrome_raster_complete");
        }

        let _present = tracing::info_span!("present", mode = "blit").entered();
        let pixels = rasterizer.read_rgba8().map_err(Box::new)?;
        if let Some(trace) = trace.as_ref() {
            trace.mark("readback_complete");
        }
        let Some(presenter) = self.presenter.as_mut() else {
            return Ok(Presented::Dropped);
        };
        presenter.resize(viewport);
        let upload_bytes = pixels.len() as u64;
        let outcome = presenter.present(&pixels, viewport.width, viewport.height)?;
        self.after_present(outcome, viewport, upload_bytes, &trace)?;
        Ok(outcome)
    }

    /// Publish this frame as retained layers, re-rasterizing and re-uploading only
    /// the region the moved layers cover.
    ///
    /// The rasterizer's surface persists between frames; an unchanged layer keeps
    /// the pixels it already has, and a frame that touches only the tab strip
    /// neither re-rasterizes nor re-uploads the page beneath it.
    fn composite(
        &mut self,
        scene: crate::Scene,
        viewport: Viewport,
        trace: &Option<crate::StartupTrace>,
    ) -> Result<Presented, PlatformError> {
        self.work.frames_started += 1;

        // Size the persistent surface. A reallocation throws away every retained
        // pixel, so it forces a whole-surface frame.
        let reallocated = match self.rasterizer.as_mut() {
            Some(rasterizer) => rasterizer
                .resize(viewport.width, viewport.height)
                .map_err(Box::new)?,
            None => {
                let new =
                    SkiaPainter::new_raster(viewport.width, viewport.height).map_err(Box::new)?;
                let _ = self.rasterizer.insert(new);
                true
            }
        };
        let forced_full = reallocated || !self.composited_once;
        let plan = plan_damage(&self.prev_layers, &scene.layers, forced_full);

        // Rasterize into the retained surface and read back only what changed.
        let upload = self.rasterize_damage(&scene, viewport, plan)?;
        if let Some(trace) = trace.as_ref() {
            trace.mark("chrome_raster_complete");
        }

        let Some(presenter) = self.presenter.as_mut() else {
            return Ok(Presented::Dropped);
        };
        presenter.resize(viewport);

        let _present = tracing::info_span!("present", mode = "composite").entered();
        let (outcome, upload_bytes) = match upload {
            DamageUpload::Unchanged => (presenter.reblit()?, 0),
            DamageUpload::Full(pixels) => {
                if let Some(trace) = trace.as_ref() {
                    trace.mark("readback_complete");
                }
                let bytes = pixels.len() as u64;
                let outcome = presenter.present(&pixels, viewport.width, viewport.height)?;
                (
                    outcome,
                    if outcome == Presented::Frame {
                        bytes
                    } else {
                        0
                    },
                )
            }
            DamageUpload::Region { pixels, rect } => {
                if let Some(trace) = trace.as_ref() {
                    trace.mark("readback_complete");
                }
                let bytes = pixels.len() as u64;
                let outcome =
                    presenter.present_rect(&pixels, viewport.width, viewport.height, rect)?;
                // A fresh staging texture cannot take a partial upload; fall back to
                // a whole frame and let the next damage be partial again.
                if outcome == Presented::Dropped {
                    (Presented::Dropped, 0)
                } else {
                    (
                        outcome,
                        if outcome == Presented::Frame {
                            bytes
                        } else {
                            0
                        },
                    )
                }
            }
        };

        // Remember this frame's layers only once it actually reached the screen;
        // a dropped frame must be recomposed, and forcing the next one full is the
        // safe way to do that.
        if outcome == Presented::Frame {
            self.prev_layers = scene
                .layers
                .iter()
                .map(|layer| (layer.id, layer.epoch, layer.rect))
                .collect();
            self.composited_once = true;
        } else {
            self.composited_once = false;
        }

        self.after_present(outcome, viewport, upload_bytes, trace)?;
        Ok(outcome)
    }

    /// Update the retained surface for `plan` and read back the region to upload.
    fn rasterize_damage(
        &mut self,
        scene: &crate::Scene,
        viewport: Viewport,
        plan: DamagePlan,
    ) -> Result<DamageUpload, PlatformError> {
        let rasterizer = self.rasterizer.as_mut().expect("rasterizer ensured");
        match plan {
            DamagePlan::Unchanged => Ok(DamageUpload::Unchanged),
            DamagePlan::Full => {
                rasterizer.reset();
                for layer in &scene.layers {
                    otlyra_gfx::render(&layer.list, rasterizer);
                }
                self.work.rasterized_pixels = self
                    .work
                    .rasterized_pixels
                    .saturating_add(u64::from(viewport.width) * u64::from(viewport.height));
                let pixels = rasterizer.read_rgba8().map_err(Box::new)?;
                Ok(DamageUpload::Full(pixels))
            }
            DamagePlan::Region(rect) => {
                let clip = otlyra_gfx::kurbo::Rect::new(
                    f64::from(rect.x),
                    f64::from(rect.y),
                    f64::from(rect.x + rect.width),
                    f64::from(rect.y + rect.height),
                );
                rasterizer.clip_to(clip);
                rasterizer.clear_rect(clip);
                for layer in &scene.layers {
                    if layer.rect.intersects(&rect) {
                        otlyra_gfx::render(&layer.list, rasterizer);
                    }
                }
                rasterizer.reset_clip();
                self.work.rasterized_pixels = self
                    .work
                    .rasterized_pixels
                    .saturating_add(u64::from(rect.width) * u64::from(rect.height));
                let pixels = rasterizer
                    .read_rgba8_rect(rect.x, rect.y, rect.width, rect.height)
                    .map_err(Box::new)?;
                let rect = crate::present::DamageRect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                };
                Ok(DamageUpload::Region { pixels, rect })
            }
        }
    }

    /// Frame bookkeeping shared by the whole-surface and composited paths: upload
    /// accounting, the first-frame startup report, the per-frame log, and the
    /// accessibility push that describes what is now on screen.
    fn after_present(
        &mut self,
        outcome: Presented,
        viewport: Viewport,
        upload_bytes: u64,
        trace: &Option<crate::StartupTrace>,
    ) -> Result<(), PlatformError> {
        if outcome == Presented::Frame {
            if let Some(trace) = trace.as_ref() {
                trace.mark("upload_complete");
            }
            self.work.uploaded_bytes = self.work.uploaded_bytes.saturating_add(upload_bytes);
            self.frames += 1;
            self.work.frames_presented += 1;
            let now = std::time::Instant::now();
            self.record_first_present(now, viewport, trace)?;
            self.log_frame();
        }

        // After the frame, because the tree describes what is now on screen.
        if let Some(update) = self.painter.accessibility()
            && let Some(a11y) = self.a11y.as_mut()
        {
            a11y.update(update);
            self.work.accessibility_updates += 1;
        }
        Ok(())
    }

    /// Record the first presented frame's timings and write the startup report.
    fn record_first_present(
        &mut self,
        now: std::time::Instant,
        viewport: Viewport,
        trace: &Option<crate::StartupTrace>,
    ) -> Result<(), PlatformError> {
        if self.first_frame.is_some() {
            return Ok(());
        }
        self.first_frame = Some(now);
        if let Some(trace) = trace.as_ref() {
            trace.mark("first_present_complete");
        }
        let elapsed = now.saturating_duration_since(self.startup_origin);
        tracing::info!(
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            visible_ms = self
                .window_visible
                .map(|visible| now.saturating_duration_since(visible).as_secs_f64() * 1000.0),
            "first chrome frame presented; browser interactive"
        );
        if let Some(path) = self.config.startup_report.as_deref() {
            let visible_ms = self.window_visible.map(|visible| {
                visible
                    .saturating_duration_since(self.startup_origin)
                    .as_secs_f64()
                    * 1000.0
            });
            let stages = trace.as_ref().map(|t| t.stages()).unwrap_or_default();
            let report = startup_report_json(
                elapsed.as_secs_f64() * 1000.0,
                visible_ms,
                viewport,
                &stages,
            );
            std::fs::write(path, report).map_err(PlatformError::StartupReport)?;
            self.startup_report_written = true;
        }
        Ok(())
    }

    /// Log the cumulative work counters for one presented frame.
    fn log_frame(&self) {
        let painter_work = self.painter.work_counters();
        tracing::debug!(
            frame = self.frames,
            events = self.work.events_delivered,
            redraw_requests = self.work.redraw_requests,
            frames_started = self.work.frames_started,
            frames_presented = self.work.frames_presented,
            accessibility_updates = self.work.accessibility_updates,
            rasterized_pixels = self.work.rasterized_pixels,
            uploaded_bytes = self.work.uploaded_bytes,
            chrome_reconciles = painter_work.chrome_reconciles,
            chrome_layouts = painter_work.chrome_layouts,
            chrome_paints = painter_work.chrome_paints,
            chrome_semantics = painter_work.chrome_semantics,
            page_paints = painter_work.page_paints,
            "frame presented"
        );
    }
}

/// The plan for one composited frame: what to re-rasterize and re-upload.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DamagePlan {
    /// Nothing changed; re-present the retained frame.
    Unchanged,
    /// One region moved; re-rasterize and re-upload just it.
    Region(crate::LayerRect),
    /// Redraw the whole surface — first frame, a resize, or a structural change.
    Full,
}

/// What [`WindowedApp::rasterize_damage`] produced for the presenter to upload.
enum DamageUpload {
    /// Re-present the retained frame with no upload.
    Unchanged,
    /// The whole frame, tightly packed.
    Full(Vec<u8>),
    /// One rectangle, tightly packed, and where it goes.
    Region {
        pixels: Vec<u8>,
        rect: crate::present::DamageRect,
    },
}

/// Decide what a composited frame must redraw from the previous frame's layers.
///
/// A structural change — a different number of layers, or a layer identity in a
/// different slot — forces a whole frame, because the retained surface can no
/// longer be trusted position-for-position. Otherwise the damage is the union of
/// the old and new rectangles of every layer whose epoch or rectangle moved.
fn plan_damage(
    prev: &[(crate::LayerId, u64, crate::LayerRect)],
    next: &[crate::SceneLayer],
    forced_full: bool,
) -> DamagePlan {
    if forced_full || prev.len() != next.len() {
        return DamagePlan::Full;
    }
    let mut damage: Option<crate::LayerRect> = None;
    for (previous, layer) in prev.iter().zip(next.iter()) {
        if previous.0 != layer.id {
            return DamagePlan::Full;
        }
        if previous.1 != layer.epoch || previous.2 != layer.rect {
            let moved = previous.2.union(&layer.rect);
            damage = Some(match damage {
                Some(current) => current.union(&moved),
                None => moved,
            });
        }
    }
    match damage {
        Some(rect) => DamagePlan::Region(rect),
        None => DamagePlan::Unchanged,
    }
}

impl ApplicationHandler<UserEvent> for WindowedApp<'_> {
    /// Drain cross-thread work and turn a reached deadline into one redraw.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Anything a screen reader asked for arrived on its own thread and was
        // queued. Drained here, between batches of window events, so it reaches
        // the painter as an ordinary event on the loop's thread like every other.
        if let Some(a11y) = self.a11y.as_ref() {
            for (node, action) in a11y.take_actions() {
                self.deliver(PlatformEvent::AccessibilityRequest { node, action });
            }
        }

        if self.scheduler.wake_due(std::time::Instant::now()) {
            self.issue_redraw();
        }
        self.sync_control_flow(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(id) => {
                tracing::debug!(id = ?id, "menu command");
                self.deliver(PlatformEvent::MenuCommand(id));
            }
            UserEvent::Woken => self.deliver(PlatformEvent::Woken),
            UserEvent::PresenterInstanceReady(instance) => {
                self.mark("wgpu_instance_ready");
                let Some(window) = self.window.as_ref().map(Arc::clone) else {
                    return;
                };
                let viewport = self.viewport();
                let seed = match Presenter::prepare(*instance, window) {
                    Ok(seed) => seed,
                    Err(error) => {
                        self.fail(event_loop, error.into());
                        return;
                    }
                };
                self.mark("surface_attached");
                let proxy = self.presenter_proxy.clone();
                std::thread::spawn(move || {
                    let result = Presenter::new(seed, viewport);
                    let _ = proxy.send_event(UserEvent::PresenterReady(Box::new(result)));
                });
            }
            UserEvent::PresenterReady(result) => match *result {
                Ok(presenter) => {
                    self.mark("gpu_ready");
                    self.presenter = Some(presenter);
                    self.request_frame(FrameRequest::Now);
                }
                Err(error) => self.fail(event_loop, error.into()),
            },
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // macOS can resume an already-running application; recreating the
            // window here would orphan the surface.
            return;
        }

        self.mark("event_loop_resumed");

        // Both before the window, so the icon and menu bar are in place the moment
        // anything is on screen.
        if let Some(icon) = self.config.icon {
            crate::icon::set_dock_icon(icon);
        }
        self.mark("icon_ready");

        if !self.config.menu_bar.menus.is_empty() {
            match NativeMenu::install(&self.config.menu_bar) {
                Ok(menu) => self.menu = Some(menu),
                Err(error) => {
                    self.fail(event_loop, error.into());
                    return;
                }
            }
        }
        self.mark("menu_ready");

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
        self.mark("window_created");

        // AccessKit requires the window to remain hidden until its adapter is
        // attached. GPU initialization has no such requirement and is much
        // slower, so it starts only after the hidden-window work is complete.
        self.a11y = Some(Accessibility::new(event_loop, &window));
        self.mark("accesskit_attached");
        // The painter may already want the window pinned — a saved preference
        // says Dark — and that has to land before the titlebar is ever seen.
        if let Some(wanted) = self.painter.window_appearance() {
            window.set_theme(Some(match wanted {
                crate::ColorScheme::Light => winit::window::Theme::Light,
                crate::ColorScheme::Dark => winit::window::Theme::Dark,
            }));
            self.pinned_scheme = Some(wanted);
        } else if let Some(scheme) = window.theme().map(translate_theme) {
            // What the environment is *now*, before the first frame: without
            // this the embedder would draw its first frame in the default
            // palette and switch on the first change, which reads as a flash.
            self.work.events_delivered += 1;
            let _ = self
                .painter
                .handle_event(PlatformEvent::AppearanceChanged(scheme));
        }

        let viewport = {
            let size = window.inner_size();
            Viewport::new(size.width, size.height, window.scale_factor())
        };
        self.window = Some(Arc::clone(&window));
        self.work.events_delivered += 1;
        let _ = self
            .painter
            .handle_event(PlatformEvent::SurfaceReady(viewport));

        window.set_visible(true);
        let visible = std::time::Instant::now();
        self.window_visible = Some(visible);
        self.mark("visibility_requested");
        tracing::info!(
            elapsed_ms = visible
                .saturating_duration_since(self.startup_origin)
                .as_secs_f64()
                * 1000.0,
            width = viewport.width,
            height = viewport.height,
            scale_factor = viewport.scale_factor,
            "window visible; gpu initialization continues in background"
        );

        let proxy = self.presenter_proxy.clone();
        let gpu_window = Arc::clone(&window);
        std::thread::spawn(move || {
            let instance = Presenter::instance(gpu_window);
            let _ = proxy.send_event(UserEvent::PresenterInstanceReady(Box::new(instance)));
        });
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
                let _ = self.painter.handle_event(PlatformEvent::CloseRequested);
                event_loop.exit();
            }
            // Both mean the same thing above us: the drawable changed. winit
            // separates them because scale and pixel size can change alone.
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                let viewport = self.viewport();
                self.work.events_delivered += 1;
                let request = self.painter.handle_event(PlatformEvent::Resized(viewport));
                self.request_frame(match request {
                    FrameRequest::None => FrameRequest::Now,
                    request => request,
                });
            }
            // The window became visible or came to the front. Both mean the last
            // frame may never have reached the screen — a swapchain hands back an
            // occluded texture and the paint goes nowhere — and in a loop that
            // blocks in `Wait` nothing else will ask for another one.
            WindowEvent::Occluded(false) | WindowEvent::Focused(true) => {
                self.dropped_frames = 0;
                self.request_frame(FrameRequest::Now);
            }
            WindowEvent::ThemeChanged(theme) => {
                // While the window is pinned, this event is our own pin coming
                // back, not the system changing its mind — reporting it would
                // poison what *System* later resumes following.
                if self.pinned_scheme.is_none() {
                    self.deliver(PlatformEvent::AppearanceChanged(translate_theme(theme)));
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
                self.pointer = (position.x / scale, position.y / scale);
                self.deliver(PlatformEvent::PointerMoved {
                    x: self.pointer.0,
                    y: self.pointer.1,
                });
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == winit::event::MouseButton::Left {
                    let event = match state {
                        winit::event::ElementState::Pressed => {
                            let clicks = self.count_click();
                            PlatformEvent::PointerPressed { clicks }
                        }
                        winit::event::ElementState::Released => PlatformEvent::PointerReleased,
                    };
                    self.deliver(event);
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
                self.deliver(scroll_event(delta, scale));
            }
            WindowEvent::RedrawRequested => match self.redraw() {
                Err(error) => self.fail(event_loop, error),
                Ok(Presented::Frame | Presented::Occluded) => {
                    self.dropped_frames = 0;
                    if self.startup_report_written {
                        event_loop.exit();
                        return;
                    }
                    self.request_frame(self.painter.next_frame());
                    self.sync_control_flow(event_loop);
                }
                // Ask for the frame again. Bounded, because a swapchain that fails
                // forever must not turn the blocking loop into a spinning one — that
                // would trade a black window for a hot CPU.
                Ok(Presented::Dropped) => {
                    self.dropped_frames += 1;
                    if self.dropped_frames <= MAX_DROPPED_FRAMES {
                        self.request_frame(FrameRequest::Now);
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

/// Stable wire format consumed by the startup benchmark runner.
///
/// `stages` is the ordered milestone table, each entry milliseconds from the
/// process origin. It is emitted as an array so the aggregator keeps the launch
/// order and can turn adjacent milestones into per-stage durations.
fn startup_report_json(
    process_to_first_frame_ms: f64,
    process_to_visible_ms: Option<f64>,
    viewport: Viewport,
    stages: &[(&'static str, f64)],
) -> String {
    let visible = process_to_visible_ms
        .map(|value| format!("{value:.6}"))
        .unwrap_or_else(|| "null".to_owned());
    let visible_to_first_frame = process_to_visible_ms
        .map(|value| format!("{:.6}", process_to_first_frame_ms - value))
        .unwrap_or_else(|| "null".to_owned());
    let stages_json = if stages.is_empty() {
        "[]".to_owned()
    } else {
        let mut body = String::new();
        for (index, (name, ms)) in stages.iter().enumerate() {
            let comma = if index + 1 < stages.len() { "," } else { "" };
            body.push_str(&format!(
                "\n    {{ \"name\": \"{name}\", \"ms\": {ms:.6} }}{comma}"
            ));
        }
        format!("[{body}\n  ]")
    };
    format!(
        concat!(
            "{{\n",
            "  \"schema\": 2,\n",
            "  \"process_to_visible_ms\": {visible},\n",
            "  \"process_to_first_frame_ms\": {process_to_first_frame_ms:.6},\n",
            "  \"visible_to_first_frame_ms\": {visible_to_first_frame},\n",
            "  \"physical_width\": {width},\n",
            "  \"physical_height\": {height},\n",
            "  \"scale_factor\": {scale_factor:.6},\n",
            "  \"stages\": {stages}\n",
            "}}\n"
        ),
        visible = visible,
        process_to_first_frame_ms = process_to_first_frame_ms,
        visible_to_first_frame = visible_to_first_frame,
        width = viewport.width,
        height = viewport.height,
        scale_factor = viewport.scale_factor,
        stages = stages_json,
    )
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
    fn startup_report_is_stable_machine_readable_json() {
        let stages = [
            ("main_entered", 0.5),
            ("visibility_requested", 40.0),
            ("first_present_complete", 91.25),
        ];
        let report =
            startup_report_json(91.25, Some(40.0), Viewport::new(2048, 1536, 2.0), &stages);
        let value: serde_json::Value = serde_json::from_str(&report).expect("valid JSON");

        assert_eq!(value["schema"], 2);
        assert_eq!(value["process_to_visible_ms"], 40.0);
        assert_eq!(value["process_to_first_frame_ms"], 91.25);
        assert_eq!(value["visible_to_first_frame_ms"], 51.25);
        assert_eq!(value["physical_width"], 2048);
        assert_eq!(value["physical_height"], 1536);
        assert_eq!(value["scale_factor"], 2.0);

        let recorded = value["stages"].as_array().expect("stages is an array");
        assert_eq!(recorded.len(), 3);
        assert_eq!(recorded[0]["name"], "main_entered");
        assert_eq!(recorded[0]["ms"], 0.5);
        assert_eq!(recorded[2]["name"], "first_present_complete");
        assert_eq!(recorded[2]["ms"], 91.25);
    }

    #[test]
    fn a_startup_trace_records_each_milestone_once_in_order() {
        let trace = crate::StartupTrace::new(std::time::Instant::now());
        trace.mark("main_entered");
        trace.mark("window_created");
        // A path that runs twice must not double-count its milestone.
        trace.mark("window_created");
        let stages = trace.stages();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].0, "main_entered");
        assert_eq!(stages[1].0, "window_created");
        assert!(stages[0].1 <= stages[1].1);
    }

    #[test]
    fn a_startup_report_without_a_trace_emits_an_empty_stage_array() {
        let report = startup_report_json(10.0, None, Viewport::new(2, 2, 1.0), &[]);
        let value: serde_json::Value = serde_json::from_str(&report).expect("valid JSON");
        assert_eq!(value["process_to_visible_ms"], serde_json::Value::Null);
        assert_eq!(value["stages"].as_array().expect("array").len(), 0);
    }

    fn layer(id: u64, epoch: u64, rect: crate::LayerRect) -> crate::SceneLayer {
        crate::SceneLayer {
            id: crate::LayerId(id),
            rect,
            epoch,
            list: std::sync::Arc::new(otlyra_gfx::DisplayList::new()),
        }
    }

    fn rect(x: u32, y: u32, w: u32, h: u32) -> crate::LayerRect {
        crate::LayerRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_forced_full_frame_redraws_everything() {
        let next = [layer(0, 1, rect(0, 0, 10, 10))];
        assert_eq!(plan_damage(&[], &next, true), DamagePlan::Full);
    }

    #[test]
    fn a_different_layer_count_or_identity_redraws_everything() {
        let prev = [(crate::LayerId(0), 1, rect(0, 0, 10, 10))];
        let two = [
            layer(0, 1, rect(0, 0, 10, 10)),
            layer(1, 1, rect(0, 10, 10, 10)),
        ];
        assert_eq!(plan_damage(&prev, &two, false), DamagePlan::Full);

        let reordered = [layer(9, 1, rect(0, 0, 10, 10))];
        assert_eq!(plan_damage(&prev, &reordered, false), DamagePlan::Full);
    }

    #[test]
    fn an_unchanged_frame_damages_nothing() {
        let prev = [
            (crate::LayerId(0), 5, rect(0, 20, 100, 80)),
            (crate::LayerId(1), 2, rect(0, 0, 100, 20)),
        ];
        let next = [
            layer(0, 5, rect(0, 20, 100, 80)),
            layer(1, 2, rect(0, 0, 100, 20)),
        ];
        assert_eq!(plan_damage(&prev, &next, false), DamagePlan::Unchanged);
    }

    #[test]
    fn one_changed_layer_damages_only_its_rectangle() {
        // Page (id 0) below, chrome (id 1) on top. Only the chrome epoch moves:
        // the page keeps its pixels, so damage is the chrome rect alone.
        let prev = [
            (crate::LayerId(0), 5, rect(0, 20, 100, 80)),
            (crate::LayerId(1), 2, rect(0, 0, 100, 20)),
        ];
        let next = [
            layer(0, 5, rect(0, 20, 100, 80)),
            layer(1, 3, rect(0, 0, 100, 20)),
        ];
        assert_eq!(
            plan_damage(&prev, &next, false),
            DamagePlan::Region(rect(0, 0, 100, 20))
        );
    }

    #[test]
    fn a_moved_rectangle_damages_both_where_it_was_and_where_it_is() {
        let prev = [(crate::LayerId(0), 1, rect(0, 0, 10, 10))];
        let next = [layer(0, 1, rect(20, 0, 10, 10))];
        assert_eq!(
            plan_damage(&prev, &next, false),
            DamagePlan::Region(rect(0, 0, 30, 10))
        );
    }

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

    #[test]
    fn immediate_frame_requests_are_coalesced_until_the_redraw_starts() {
        let now = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();

        assert!(scheduler.request(FrameRequest::Now, now));
        assert!(
            !scheduler.request(FrameRequest::Now, now),
            "winit already has one redraw request"
        );

        scheduler.redraw_started();
        assert!(
            scheduler.request(FrameRequest::Now, now),
            "a change during or after that frame needs the next one"
        );
    }

    #[test]
    fn the_earliest_deadline_wins_and_wakes_once() {
        let now = std::time::Instant::now();
        let later = now + std::time::Duration::from_secs(2);
        let sooner = now + std::time::Duration::from_secs(1);
        let mut scheduler = FrameScheduler::default();

        assert!(!scheduler.request(FrameRequest::At(later), now));
        assert!(!scheduler.request(FrameRequest::At(sooner), now));
        assert_eq!(scheduler.deadline, Some(sooner));
        assert!(!scheduler.wake_due(now));
        assert!(scheduler.wake_due(sooner));
        assert_eq!(scheduler.deadline, None);
        assert!(
            !scheduler.wake_due(later),
            "a reached deadline is consumed exactly once"
        );
    }

    #[test]
    fn a_vsync_request_becomes_a_future_deadline() {
        let now = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();

        assert!(!scheduler.request(FrameRequest::Vsync, now));
        assert_eq!(scheduler.deadline, Some(now + FRAME_INTERVAL));
        assert!(!scheduler.wake_due(now));
        assert!(scheduler.wake_due(now + FRAME_INTERVAL));
    }

    #[test]
    fn an_immediate_frame_supersedes_an_old_animation_deadline() {
        let now = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();

        assert!(!scheduler.request(FrameRequest::Vsync, now));
        assert!(scheduler.deadline.is_some());
        assert!(scheduler.request(FrameRequest::Now, now));
        assert_eq!(
            scheduler.deadline, None,
            "the painter will publish a fresh deadline after this frame"
        );
    }
}
