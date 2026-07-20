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
    /// content should move up, i.e. the reader is going down the page.
    ///
    /// Line-based wheels are converted here, because how many pixels a wheel notch
    /// is worth is a platform fact and this crate is where platform facts live.
    Scroll {
        /// Horizontal delta in logical pixels.
        x: f64,
        /// Vertical delta in logical pixels.
        y: f64,
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
    PointerPressed,
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
    /// The user asked to close the window. The loop exits after this is delivered.
    CloseRequested,
    /// The user chose a menu item the embedder defined.
    MenuCommand(MenuId),
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

/// The embedder's side of the boundary: given a target and a viewport, draw.
pub trait Painter {
    /// Take the handle that wakes the loop. Called once, before the first frame.
    ///
    /// A painter that never works off the loop's own thread can ignore it, which is
    /// why it has a default.
    fn set_waker(&mut self, waker: Waker) {
        let _ = waker;
    }

    /// Whether the painter wants a frame at the display's pace rather than only
    /// when something happens.
    ///
    /// The loop otherwise blocks, so an animation that nobody asks for does not
    /// run: this is how a spinner spins without the browser burning a core when
    /// nothing is moving.
    fn animating(&self) -> bool {
        false
    }

    /// React to a platform event. Default: ignore it.
    fn on_event(&mut self, event: PlatformEvent) {
        let _ = event;
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
    /// The application menu bar. Empty means no menu bar is installed.
    pub menu_bar: MenuBar,
    /// Encoded image (PNG) to show in the Dock or taskbar.
    ///
    /// Needed because an unbundled binary has no plist to read an icon from, which
    /// is exactly the `cargo run` case.
    pub icon: Option<&'static [u8]>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "Otlyra".to_owned(),
            logical_size: (1024.0, 768.0),
            menu_bar: MenuBar::new(),
            icon: None,
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
