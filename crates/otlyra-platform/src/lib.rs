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

mod a11y;
mod event_loop;
mod icon;
mod menu;
mod present;

pub use event_loop::{PlatformError, run};
pub use menu::{Menu, MenuBar, MenuEntry, MenuError, MenuId, SystemItem};

use otlyra_gfx::PaintTarget;
use std::time::Instant;

/// The accessibility vocabulary, re-exported so the browser names the same types
/// the platform hands on.
///
/// Not a `winit` type and not a `wgpu` one: `accesskit` is a vocabulary crate the
/// way `kurbo` and `peniko` are, and translating it into a second tree of our own
/// would be a hundred lines that can only ever fall behind it.
pub use accesskit;

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

/// What a scroll came from.
///
/// The two are not the same gesture wearing different hardware. A wheel sends
/// *notches*: a handful of discrete events, each worth a jump of some fixed
/// distance the platform decides. A trackpad sends a *distance*, in a stream of
/// small precise deltas, followed by momentum after the fingers have left. Treat
/// a three-pixel trackpad delta as a notch and the page leaps; treat a notch as
/// three pixels and the wheel does nothing.
///
/// It matters for direction too, on macOS. The system applies its natural
/// scrolling preference to the delta before it reaches us, and there is no way
/// to ask which way it decided — so a browser that wants to offer its own
/// preference has to know which device the delta came from to apply it to the
/// right one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScrollSource {
    /// A mouse wheel, in notches converted to pixels.
    Wheel,
    /// A trackpad or another precise device, already in pixels.
    Trackpad,
}

/// What assistive technology asked to be done.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AccessibilityAction {
    /// Press it: the same intent a click carries.
    Activate,
    /// Put the keyboard on it.
    Focus,
    /// Move it one step up its range: what a reader asks of a slider.
    Increment,
    /// Move it one step down.
    Decrement,
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
    /// The user scrolled, by this many **logical** pixels. Positive `y` means the
    /// reader is going *down* the page — the scroll offset grows.
    ///
    /// One convention, stated once, and every consumer adds it to an offset
    /// rather than deciding for itself which way is down. Two consumers that
    /// each negated it their own way is exactly how a browser ends up scrolling
    /// one direction on a document and the other on its own settings.
    ///
    /// Line-based wheels are converted here, because how many pixels a wheel notch
    /// is worth is a platform fact and this crate is where platform facts live.
    Scroll {
        /// Horizontal delta in logical pixels.
        x: f64,
        /// Vertical delta in logical pixels.
        y: f64,
        /// What the reader scrolled with.
        source: ScrollSource,
    },
    /// The pointer moved to this position, in logical pixels from the top left of
    /// the drawable.
    PointerMoved {
        /// Horizontal position.
        x: f64,
        /// Vertical position.
        y: f64,
    },
    /// The primary pointer button went down at the last reported position.
    PointerPressed {
        /// How many presses this is the latest of: `1` for a click, `2` for a
        /// double-click, `3` for a triple, counted while the presses stay close
        /// in time and place. A platform fact, because how close is *close
        /// enough* is a platform convention and this crate is where platform
        /// conventions live.
        clicks: u32,
    },
    /// The primary pointer button came up.
    PointerReleased,
    /// A key went down.
    KeyPressed {
        /// Which key, in a vocabulary that does not depend on the keyboard layout.
        key: Key,
        /// Modifiers held at the time.
        modifiers: Modifiers,
    },
    /// The user typed text. Separate from [`PlatformEvent::KeyPressed`] because
    /// what a key *inserts* depends on layout, dead keys and the input method,
    /// and the answer is the platform's to give.
    TextInput(char),
    /// The window's environment is light or dark, at creation and whenever the
    /// person switches it. The embedder decides what, if anything, follows.
    AppearanceChanged(ColorScheme),
    /// The user asked to close the window. The loop exits after this is delivered.
    CloseRequested,
    /// The user chose a menu item the embedder defined.
    MenuCommand(MenuId),
    /// Assistive technology asked for something, naming what by the identifier the
    /// embedder gave it in the accessibility tree.
    ///
    /// Two of them, because two are what a page without a script can answer: press
    /// this, and put the keyboard here. Both carry the same intent the pointer and
    /// the keyboard already carry, so they join that route rather than opening a
    /// third way into the same code. The rest need a vocabulary this layer does not
    /// have — scroll a named node into view — or are already the platform's job.
    AccessibilityRequest {
        /// Which node the reader named.
        node: accesskit::NodeId,
        /// What it asked for.
        action: AccessibilityAction,
    },
    /// Something outside the loop asked for attention: a [`Waker`] was woken, or
    /// the painter said it was animating and this is the next tick. What that means
    /// is the painter's business; the loop only knows a frame is wanted.
    Woken,
}

/// A handle that wakes the event loop from another thread.
///
/// The one thing a worker thread may do to the interface: say that something has
/// finished. Everything else — what finished, what it means — stays on the thread
/// the loop runs on, because that is where the state it changes lives.
#[derive(Clone, Debug)]
pub struct Waker(std::sync::mpsc::Sender<()>);

impl Waker {
    /// Build a waker over a channel the loop drains. Used by the loop itself; an
    /// embedder is given one rather than making it.
    pub fn new(sender: std::sync::mpsc::Sender<()>) -> Self {
        Self(sender)
    }

    /// Ask the loop for a frame. Cheap, and safe to call from any thread; a wake
    /// after the loop has gone is silently dropped.
    pub fn wake(&self) {
        let _ = self.0.send(());
    }
}

/// The keys the browser acts on by identity rather than by what they type.
///
/// Deliberately small. A key with no consumer is a translation this layer would
/// have to keep honest for nothing; the rest arrive as
/// [`PlatformEvent::TextInput`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Key {
    /// Return or Enter.
    Enter,
    /// Backspace.
    Backspace,
    /// Delete forward.
    Delete,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// F5, which reloads on every platform that has the key.
    F5,
    /// A printable character, identified by what an unmodified press would type.
    /// Used for shortcuts: `Cmd+T` arrives as `Character('t')`.
    Character(char),
}

/// Whether the environment around the window is light or dark.
///
/// Two values and not three: *system* is a policy about which of these to use,
/// and policies belong to the embedder. This layer only reports what the
/// platform says the environment is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    /// A light environment.
    Light,
    /// A dark one.
    Dark,
}

/// Modifier keys held during an event.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub control: bool,
    /// Alt, or Option.
    pub alt: bool,
    /// The platform's command key: Command on macOS, the Windows key elsewhere.
    pub command: bool,
}

impl Modifiers {
    /// Whether this is the platform's "do the menu action" modifier and nothing
    /// else — Command on macOS, Control elsewhere.
    pub fn is_accelerator(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.command && !self.control && !self.alt
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.control && !self.command && !self.alt
        }
    }
}

/// What the pointer should look like.
///
/// Three, because three is what the browser can currently justify: a link, text,
/// and everything else. Each one is a promise that the thing under the pointer
/// behaves that way.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Cursor {
    /// The ordinary arrow.
    #[default]
    Default,
    /// Over something that navigates when clicked.
    Pointer,
    /// Over text.
    Text,
}

/// When the embedder needs another frame.
///
/// Requests are coalesced by the platform loop. `Now` is for a state change that
/// is already visible, `Vsync` for a continuous animation, and `At` for something
/// with a known transition time such as a caret blink.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum FrameRequest {
    /// Nothing visible changed.
    #[default]
    None,
    /// Present the changed state as soon as the platform can.
    Now,
    /// Present on the next display-paced animation tick.
    Vsync,
    /// Wake for a transition at this instant.
    At(Instant),
}

/// Cumulative work performed above the platform paint seam.
///
/// Legacy chrome builds, lays out, and paints together; retained roots advance
/// only the counters for work they actually perform.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PainterWork {
    /// Browser-owned UI trees built or reconciled.
    pub chrome_reconciles: u64,
    /// Browser-owned UI layout passes.
    pub chrome_layouts: u64,
    /// Browser-owned UI display lists built.
    pub chrome_paints: u64,
    /// Browser-owned semantic descriptions built.
    pub chrome_semantics: u64,
    /// Page display lists built rather than reused.
    pub page_paints: u64,
}

/// A stable identity for one retained scene layer.
///
/// The compositor keys a layer's retained pixels and version on this, so it must
/// stay the same across frames for the same conceptual surface (the tab strip,
/// the page, the inspector) and differ between them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LayerId(pub u64);

/// One layer's device-pixel rectangle on the window surface.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LayerRect {
    /// Left edge, device pixels from the surface's left.
    pub x: u32,
    /// Top edge, device pixels from the surface's top.
    pub y: u32,
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
}

impl LayerRect {
    /// A rectangle covering the whole viewport.
    pub fn of_viewport(viewport: Viewport) -> Self {
        Self {
            x: 0,
            y: 0,
            width: viewport.width,
            height: viewport.height,
        }
    }

    /// Nothing to draw — zero on either axis.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    fn right(&self) -> u32 {
        self.x + self.width
    }

    fn bottom(&self) -> u32 {
        self.y + self.height
    }

    /// Whether the two rectangles share any pixel.
    pub fn intersects(&self, other: &LayerRect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// The smallest rectangle covering both. An empty operand contributes nothing.
    pub fn union(&self, other: &LayerRect) -> LayerRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let width = self.right().max(other.right()) - x;
        let height = self.bottom().max(other.bottom()) - y;
        LayerRect {
            x,
            y,
            width,
            height,
        }
    }
}

/// One retained layer: its identity, where it sits, a content version, and the
/// device-space display list that draws it.
///
/// `epoch` changes exactly when `list` would draw something different. The
/// compositor re-rasterizes and re-uploads a layer only when its epoch or rect
/// moved, so a layer whose epoch is unchanged costs nothing.
pub struct SceneLayer {
    /// Stable identity across frames.
    pub id: LayerId,
    /// Where the layer sits on the surface, in device pixels.
    pub rect: LayerRect,
    /// Content version; changes exactly when `list` would draw differently.
    pub epoch: u64,
    /// The device-space display list that draws the layer.
    pub list: otlyra_gfx::DisplayList,
}

/// One frame as an ordered, back-to-front stack of retained layers.
///
/// A [`Painter`] that returns `Some(Scene)` from [`Painter::compose`] opts into
/// the retained compositor; returning `None` keeps the whole-surface paint path.
pub struct Scene {
    /// The layers, back to front.
    pub layers: Vec<SceneLayer>,
}

/// The embedder's side of the boundary: given a target and a viewport, draw.
pub trait Painter {
    /// Publish this frame as retained layers, or `None` to use [`Painter::paint`].
    ///
    /// The compositor re-rasterizes and re-uploads only the layers whose epoch or
    /// rect changed since the last frame, so an input that touches one layer
    /// leaves the others' pixels untouched. A painter that has not adopted layers
    /// returns `None` and is drawn whole through `paint`.
    fn compose(&mut self, viewport: Viewport) -> Option<Scene> {
        let _ = viewport;
        None
    }

    /// Take the handle that wakes the loop. Called once, before the first frame.
    ///
    /// A painter that never works off the loop's own thread can ignore it, which is
    /// why it has a default.
    fn set_waker(&mut self, waker: Waker) {
        let _ = waker;
    }

    /// React to a platform event. Default: ignore it.
    fn on_event(&mut self, event: PlatformEvent) {
        let _ = event;
    }

    /// Deliver a platform event and say whether it changed visible output.
    ///
    /// The loop blocks when no frame is requested. Returning [`FrameRequest::None`]
    /// is therefore both a correctness statement and a performance contract. The
    /// default preserves the old event contract; painters opt into narrower
    /// scheduling by overriding this method.
    fn handle_event(&mut self, event: PlatformEvent) -> FrameRequest {
        self.on_event(event);
        FrameRequest::Now
    }

    /// When a frame should follow the one just presented.
    ///
    /// This is separate from [`Painter::on_event`] because an animation or a
    /// deadline continues without another input event.
    fn next_frame(&self) -> FrameRequest {
        FrameRequest::None
    }

    /// Cumulative work above this seam, for frame diagnostics.
    fn work_counters(&self) -> PainterWork {
        PainterWork::default()
    }

    /// The accessibility tree, if it has changed since the last frame.
    ///
    /// A browser has to build this out of the DOM whatever toolkit it uses, so it
    /// is asked for here rather than derived from anything this crate knows.
    /// `None` means "unchanged", not "nothing to expose".
    fn accessibility(&mut self) -> Option<accesskit::TreeUpdate> {
        None
    }

    /// What the pointer should look like now.
    ///
    /// Polled after every event rather than pushed, because the answer is a
    /// function of state the painter already keeps — where the pointer is and what
    /// is under it — and a push would be a second copy of that.
    fn cursor(&self) -> Cursor {
        Cursor::Default
    }

    /// The scheme the *window itself* — its titlebar — should wear, or `None`
    /// to follow the system.
    ///
    /// Polled like the cursor and for the same reason. This exists because an
    /// embedder that draws itself dark under a light titlebar looks broken in a
    /// way no amount of its own drawing can fix; the titlebar is the
    /// platform's, so the platform has to be told.
    fn window_appearance(&self) -> Option<ColorScheme> {
        None
    }

    /// Paint one frame. `target` has already been reset for this frame.
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport);
}

/// Records ordered process-origin milestones for the startup benchmark.
///
/// One handle is cloned across the app bootstrap, the event-loop thread, and the
/// GPU worker threads. Every milestone is stored as milliseconds from a single
/// process origin, so the benchmark can attribute the whole launch timeline to
/// named stages rather than to the three coarse spans the report used to carry.
///
/// Interior mutability, because the marks come from wherever the milestone is
/// actually reached and the loop only owns `&self` at most of those points.
#[derive(Clone)]
pub struct StartupTrace {
    origin: Instant,
    stages: std::sync::Arc<std::sync::Mutex<Vec<(&'static str, f64)>>>,
}

impl StartupTrace {
    /// A fresh trace measured from `origin`, the same instant the coarse spans use.
    pub fn new(origin: Instant) -> Self {
        Self {
            origin,
            stages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Record `name` at the current instant, in milliseconds from the origin.
    ///
    /// The first mark for a name wins: a milestone reached on a path that can run
    /// twice — a resume that fires again, a first frame retried after a dropped
    /// swapchain — is not double-counted.
    pub fn mark(&self, name: &'static str) {
        let ms = self.origin.elapsed().as_secs_f64() * 1000.0;
        let mut stages = self.stages.lock().expect("startup trace poisoned");
        if stages.iter().any(|(existing, _)| *existing == name) {
            return;
        }
        stages.push((name, ms));
    }

    /// The milestones recorded so far, in the order they were reached.
    pub fn stages(&self) -> Vec<(&'static str, f64)> {
        self.stages.lock().expect("startup trace poisoned").clone()
    }
}

impl std::fmt::Debug for StartupTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartupTrace")
            .field("stages", &self.stages())
            .finish()
    }
}

/// How the window should be created.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    /// Title bar text.
    pub title: String,
    /// Initial size in *logical* pixels — this is the one place logical units are
    /// the natural unit, because that is what the user and the OS agree on.
    pub logical_size: (f64, f64),
    /// The application menu bar. Empty means no menu bar is installed.
    pub menu_bar: MenuBar,
    /// Encoded image (PNG) to show in the Dock or taskbar.
    ///
    /// Needed because an unbundled binary has no plist to read an icon from, which
    /// is exactly the `cargo run` case.
    pub icon: Option<&'static [u8]>,
    /// The origin used for startup milestones.
    ///
    /// The executable sets this at the beginning of `main`; other embedders get
    /// a useful run-to-first-frame measurement from [`WindowConfig::default`].
    pub startup_origin: Instant,
    /// Write machine-readable startup timings after the first presented frame,
    /// then close the window.
    ///
    /// This is a benchmark probe, not ordinary application logging. A dedicated
    /// file keeps the measurement independent of tracing format and log level.
    pub startup_report: Option<std::path::PathBuf>,
    /// Per-stage startup milestones, recorded from `startup_origin`.
    ///
    /// The executable seeds this with its bootstrap milestones and shares the same
    /// handle here; the loop records the rest and folds them into the startup
    /// report. `None` for embedders that only want the coarse spans.
    pub startup_trace: Option<StartupTrace>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "Otlyra".to_owned(),
            logical_size: (1024.0, 768.0),
            menu_bar: MenuBar::new(),
            icon: None,
            startup_origin: Instant::now(),
            startup_report: None,
            startup_trace: None,
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
        let _ = painter.handle_event(PlatformEvent::SurfaceReady(viewport));
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
