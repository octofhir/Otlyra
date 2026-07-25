//! The retained-layer compositor, and a window's frame path with no window.
//!
//! One implementation, two callers. [`SceneCompositor`] owns the persistent
//! raster surface, works out what a frame has to redraw from the layers the
//! embedder published, and re-rasterizes only that; the event loop hands the
//! result to a swapchain, and [`FramePump`] hands it back to the caller as
//! pixels. That is the point of the split: a test or a driver that wants to know
//! what the window shows runs the *same* `compose` → damage → retained-surface
//! path the window runs, rather than a second whole-surface paint that can agree
//! with the tests and disagree with the screen.

use otlyra_gfx::{PaintTarget, SkiaPainter};

use crate::{FrameRequest, LayerId, LayerRect, Painter, PlatformEvent, Scene, Viewport};

/// What one composited frame had to redraw.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Damage {
    /// Nothing changed; the retained frame stands.
    Unchanged,
    /// One region moved; it alone was re-rasterized and re-uploaded.
    Region(LayerRect),
    /// The whole surface — first frame, a resize, or a structural change.
    Full,
}

impl Damage {
    /// Device pixels this damage covers, for the rasterization counters.
    pub(crate) fn pixels(&self, viewport: Viewport) -> u64 {
        match self {
            Damage::Unchanged => 0,
            Damage::Region(rect) => u64::from(rect.width) * u64::from(rect.height),
            Damage::Full => u64::from(viewport.width) * u64::from(viewport.height),
        }
    }

    /// Whether any pixel was redrawn.
    pub fn is_empty(&self) -> bool {
        matches!(self, Damage::Unchanged)
    }
}

/// The pixels a composited frame produced for the presenter to upload.
pub(crate) enum Upload {
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

/// One composited frame: what it redrew and the bytes that redraw produced.
pub(crate) struct Composited {
    pub damage: Damage,
    pub upload: Upload,
}

/// The persistent raster surface and the layers that are currently on it.
///
/// A layer whose epoch and rectangle are unchanged keeps the pixels it already
/// has, so a frame that touches only the toolbar neither re-rasterizes nor
/// re-uploads the page underneath it.
pub(crate) struct SceneCompositor {
    /// The retained surface. `None` until the first frame sizes it.
    surface: Option<SkiaPainter>,
    /// Last presented frame's layers — identity, epoch, and rectangle, in order.
    prev: Vec<(LayerId, u64, LayerRect)>,
    /// Whether a full composite has reached the caller. Until it has, and after
    /// any surface reallocation, the compositor owes a whole-surface frame.
    composited_once: bool,
}

impl SceneCompositor {
    pub(crate) fn new() -> Self {
        Self {
            surface: None,
            prev: Vec::new(),
            composited_once: false,
        }
    }

    /// Size the retained surface, answering whether it was (re)allocated — which
    /// throws away every retained pixel and so forces a whole-surface frame.
    fn size(&mut self, viewport: Viewport) -> Result<bool, crate::PlatformError> {
        match self.surface.as_mut() {
            Some(surface) => Ok(surface
                .resize(viewport.width, viewport.height)
                .map_err(Box::new)?),
            None => {
                let new =
                    SkiaPainter::new_raster(viewport.width, viewport.height).map_err(Box::new)?;
                let _ = self.surface.insert(new);
                Ok(true)
            }
        }
    }

    /// Draw `scene` into the retained surface and read back what changed.
    pub(crate) fn rasterize(
        &mut self,
        scene: &Scene,
        viewport: Viewport,
    ) -> Result<Composited, crate::PlatformError> {
        let reallocated = self.size(viewport)?;
        let damage = self.plan(scene, reallocated);
        let surface = self.surface.as_mut().expect("surface sized");

        let upload = match damage {
            Damage::Unchanged => Upload::Unchanged,
            Damage::Full => {
                surface.reset();
                for layer in &scene.layers {
                    otlyra_gfx::render(&layer.list, surface);
                }
                Upload::Full(surface.read_rgba8().map_err(Box::new)?)
            }
            Damage::Region(rect) => {
                let clip = otlyra_gfx::kurbo::Rect::new(
                    f64::from(rect.x),
                    f64::from(rect.y),
                    f64::from(rect.x + rect.width),
                    f64::from(rect.y + rect.height),
                );
                surface.clip_to(clip);
                surface.clear_rect(clip);
                for layer in &scene.layers {
                    if layer.rect.intersects(&rect) {
                        otlyra_gfx::render(&layer.list, surface);
                    }
                }
                surface.reset_clip();
                let pixels = surface
                    .read_rgba8_rect(rect.x, rect.y, rect.width, rect.height)
                    .map_err(Box::new)?;
                Upload::Region {
                    pixels,
                    rect: crate::present::DamageRect {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                    },
                }
            }
        };
        Ok(Composited { damage, upload })
    }

    /// Plan one frame for any retained surface backend.
    ///
    /// `surface_reallocated` says the backend has lost every retained pixel.
    /// The CPU path answers that from its Skia surface; the opt-in GPU path
    /// answers it from its wgpu texture. Keeping the layer history here means
    /// both backends use one damage algorithm.
    pub(crate) fn plan(&self, scene: &Scene, surface_reallocated: bool) -> Damage {
        plan_damage(
            &self.prev,
            &scene.layers,
            surface_reallocated || !self.composited_once,
        )
    }

    /// Draw a whole frame through [`Painter::paint`], for a painter that has not
    /// adopted layers. The surface is retained all the same, so the pixels can be
    /// read back the same way.
    pub(crate) fn rasterize_whole(
        &mut self,
        painter: &mut dyn Painter,
        viewport: Viewport,
    ) -> Result<Vec<u8>, crate::PlatformError> {
        self.size(viewport)?;
        let surface = self.surface.as_mut().expect("surface sized");
        surface.reset();
        painter.paint(surface, viewport);
        // Layers no longer describe what is on the surface.
        self.prev.clear();
        self.composited_once = false;
        Ok(surface.read_rgba8().map_err(Box::new)?)
    }

    /// Remember the layers now on the surface. Called once a frame has actually
    /// reached its destination: a frame that was dropped must be recomposed.
    pub(crate) fn commit(&mut self, scene: &Scene) {
        self.prev = scene
            .layers
            .iter()
            .map(|layer| (layer.id, layer.epoch, layer.rect))
            .collect();
        self.composited_once = true;
    }

    /// Forget that the surface is trustworthy, so the next frame is whole.
    pub(crate) fn discard(&mut self) {
        self.composited_once = false;
    }

    /// The whole retained surface as RGBA8, which is what is on the screen.
    pub(crate) fn read_rgba8(&mut self) -> Result<Vec<u8>, crate::PlatformError> {
        match self.surface.as_mut() {
            Some(surface) => Ok(surface.read_rgba8().map_err(Box::new)?),
            None => Ok(Vec::new()),
        }
    }

    /// The same, as a PNG.
    pub(crate) fn encode_png(&mut self) -> Result<Vec<u8>, crate::PlatformError> {
        match self.surface.as_mut() {
            Some(surface) => Ok(surface.encode_png().map_err(Box::new)?),
            None => Err(crate::PlatformError::Rasterizer(Box::new(
                otlyra_gfx::SkiaError::ZeroSize {
                    width: 0,
                    height: 0,
                },
            ))),
        }
    }
}

/// Decide what a composited frame must redraw from the previous frame's layers.
///
/// A structural change — a different number of layers, or a layer identity in a
/// different slot — forces a whole frame, because the retained surface can no
/// longer be trusted position-for-position. Otherwise the damage is the union of
/// the old and new rectangles of every layer whose epoch or rectangle moved.
fn plan_damage(
    prev: &[(LayerId, u64, LayerRect)],
    next: &[crate::SceneLayer],
    forced_full: bool,
) -> Damage {
    if forced_full || prev.len() != next.len() {
        return Damage::Full;
    }
    let mut damage: Option<LayerRect> = None;
    for (previous, layer) in prev.iter().zip(next.iter()) {
        if previous.0 != layer.id {
            return Damage::Full;
        }
        if previous.1 != layer.epoch || previous.2 != layer.rect {
            // A layer that stayed where it is and says which part of itself
            // changed is taken at its word; one that moved has to answer for
            // where it was as well, and its own account of what changed inside it
            // says nothing about the pixels it has left behind.
            let moved = match layer.dirty {
                Some(dirty) if previous.2 == layer.rect => clamp(dirty, layer.rect),
                _ => previous.2.union(&layer.rect),
            };
            // A layer may change content that is clipped wholly outside its
            // visible rectangle. Its epoch still advances, but there are no
            // retained pixels to replace.
            if moved.width == 0 || moved.height == 0 {
                continue;
            }
            damage = Some(match damage {
                Some(current) => current.union(&moved),
                None => moved,
            });
        }
    }
    match damage {
        Some(rect) => Damage::Region(rect),
        None => Damage::Unchanged,
    }
}

/// A layer's own account of what changed inside it, held to the layer's bounds.
///
/// A painter that reports a rectangle reaching past its layer would have the
/// compositor redraw its neighbours' pixels from its own list, which draws
/// nothing there — so the rectangle is cut to the layer rather than trusted.
fn clamp(dirty: LayerRect, layer: LayerRect) -> LayerRect {
    let x = dirty.x.max(layer.x);
    let y = dirty.y.max(layer.y);
    let right = (dirty.x + dirty.width).min(layer.x + layer.width);
    let bottom = (dirty.y + dirty.height).min(layer.y + layer.height);
    LayerRect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

/// How many frames one settle may draw before it is a runaway rather than a
/// browser catching up. Reached only by a painter that asks for an immediate
/// frame from every frame, which is a bug worth surfacing rather than looping on.
const MAX_SETTLE_FRAMES: u32 = 16;

/// A window's frame path with no window: real events in, real composited pixels
/// out.
///
/// [`crate::render_offscreen`] answers a different question — what
/// [`Painter::paint`] draws — and a regression that lives in the compositor, in
/// a layer epoch, or in the damage rectangle is invisible to it. This runs the
/// window's own path: an event is delivered through [`Painter::handle_event`], a
/// frame is drawn only if the painter asked for one, and the frame is composed
/// from retained layers into a surface that persists between frames. What comes
/// back is what a person would see.
///
/// Deterministic on purpose: no clock, no vsync, no GPU. A frame happens because
/// an event asked for one or because the caller asked for one.
pub struct FramePump {
    compositor: SceneCompositor,
    viewport: Viewport,
    /// Whether the painter has asked for an immediate frame that has not been
    /// drawn yet.
    pending: bool,
    /// What the last frame redrew.
    damage: Damage,
    /// Frames drawn since the pump opened.
    frames: u64,
}

impl FramePump {
    /// A pump for a window of `viewport` device pixels.
    pub fn new(viewport: Viewport) -> Self {
        Self {
            compositor: SceneCompositor::new(),
            viewport,
            pending: false,
            damage: Damage::Unchanged,
            frames: 0,
        }
    }

    /// Tell the painter its surface exists, as the window does before its first
    /// frame, and draw that frame.
    pub fn open(&mut self, painter: &mut dyn Painter) -> Result<Damage, crate::PlatformError> {
        self.event(painter, PlatformEvent::SurfaceReady(self.viewport));
        self.frame(painter)
    }

    /// The window this pump is standing in for.
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Resize the window. The next frame is whole, as it is on a real resize.
    pub fn resize(&mut self, painter: &mut dyn Painter, viewport: Viewport) {
        self.viewport = viewport;
        self.event(painter, PlatformEvent::SurfaceReady(viewport));
    }

    /// Deliver one event the way the window delivers it, and remember whether the
    /// painter asked for a frame.
    pub fn event(&mut self, painter: &mut dyn Painter, event: PlatformEvent) -> FrameRequest {
        let request = painter.handle_event(event);
        if matches!(request, FrameRequest::Now) {
            self.pending = true;
        }
        request
    }

    /// Whether a frame has been asked for and not yet drawn.
    pub fn frame_requested(&self) -> bool {
        self.pending
    }

    /// Frames drawn since the pump opened.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// What the last frame redrew.
    pub fn damage(&self) -> Damage {
        self.damage
    }

    /// Draw one frame now, whether or not one was asked for.
    pub fn frame(&mut self, painter: &mut dyn Painter) -> Result<Damage, crate::PlatformError> {
        self.pending = false;
        self.frames += 1;
        let viewport = self.viewport;
        let damage = match painter.compose(viewport) {
            Some(scene) => {
                let composited = self.compositor.rasterize(&scene, viewport)?;
                // The pixels are in hand, which is this pump's equivalent of
                // presenting them: the retained surface is now what is on screen.
                self.compositor.commit(&scene);
                composited.damage
            }
            None => {
                self.compositor.rasterize_whole(painter, viewport)?;
                Damage::Full
            }
        };
        self.damage = damage;
        Ok(damage)
    }

    /// Draw the frames the painter has asked for, and answer how many that was.
    ///
    /// Immediate requests only. An animation deadline — a blinking caret, a
    /// spinner — is a frame that happens *later*, and drawing it here would make
    /// a capture depend on how many times the caller happened to settle.
    pub fn settle(&mut self, painter: &mut dyn Painter) -> Result<u32, crate::PlatformError> {
        let mut drawn = 0;
        while self.pending && drawn < MAX_SETTLE_FRAMES {
            self.frame(painter)?;
            drawn += 1;
            if matches!(painter.next_frame(), FrameRequest::Now) {
                self.pending = true;
            }
        }
        Ok(drawn)
    }

    /// The window's pixels, RGBA8, row-major, `viewport.width * 4` bytes a row.
    pub fn pixels(&mut self) -> Result<Vec<u8>, crate::PlatformError> {
        self.compositor.read_rgba8()
    }

    /// The window as a PNG — a picture of the whole window, chrome included.
    pub fn png(&mut self) -> Result<Vec<u8>, crate::PlatformError> {
        self.compositor.encode_png()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(id: u64, epoch: u64, rect: LayerRect) -> crate::SceneLayer {
        crate::SceneLayer {
            id: LayerId(id),
            rect,
            epoch,
            list: std::sync::Arc::new(otlyra_gfx::DisplayList::new()),
            dirty: None,
        }
    }

    /// The same, saying which part of itself changed.
    fn partial(id: u64, epoch: u64, rect: LayerRect, dirty: LayerRect) -> crate::SceneLayer {
        crate::SceneLayer {
            dirty: Some(dirty),
            ..layer(id, epoch, rect)
        }
    }

    fn rect(x: u32, y: u32, w: u32, h: u32) -> LayerRect {
        LayerRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_forced_full_frame_redraws_everything() {
        let next = [layer(0, 1, rect(0, 0, 10, 10))];
        assert_eq!(plan_damage(&[], &next, true), Damage::Full);
    }

    #[test]
    fn a_different_layer_count_or_identity_redraws_everything() {
        let prev = [(LayerId(0), 1, rect(0, 0, 10, 10))];
        let two = [
            layer(0, 1, rect(0, 0, 10, 10)),
            layer(1, 1, rect(0, 10, 10, 10)),
        ];
        assert_eq!(plan_damage(&prev, &two, false), Damage::Full);

        let reordered = [layer(9, 1, rect(0, 0, 10, 10))];
        assert_eq!(plan_damage(&prev, &reordered, false), Damage::Full);
    }

    #[test]
    fn an_unchanged_frame_damages_nothing() {
        let prev = [
            (LayerId(0), 5, rect(0, 20, 100, 80)),
            (LayerId(1), 2, rect(0, 0, 100, 20)),
        ];
        let next = [
            layer(0, 5, rect(0, 20, 100, 80)),
            layer(1, 2, rect(0, 0, 100, 20)),
        ];
        assert_eq!(plan_damage(&prev, &next, false), Damage::Unchanged);
    }

    #[test]
    fn an_external_retained_surface_uses_the_same_history() {
        let mut compositor = SceneCompositor::new();
        let scene = crate::Scene {
            layers: vec![layer(0, 1, rect(0, 0, 100, 100))],
        };

        assert_eq!(
            compositor.plan(&scene, true),
            Damage::Full,
            "a new GPU texture has no retained pixels"
        );
        compositor.commit(&scene);
        assert_eq!(
            compositor.plan(&scene, false),
            Damage::Unchanged,
            "once presented, the backend shares the ordinary layer history"
        );
    }

    #[test]
    fn one_changed_layer_damages_only_its_rectangle() {
        // Page (id 0) below, chrome (id 1) on top. Only the chrome epoch moves:
        // the page keeps its pixels, so damage is the chrome rect alone.
        let prev = [
            (LayerId(0), 5, rect(0, 20, 100, 80)),
            (LayerId(1), 2, rect(0, 0, 100, 20)),
        ];
        let next = [
            layer(0, 5, rect(0, 20, 100, 80)),
            layer(1, 3, rect(0, 0, 100, 20)),
        ];
        assert_eq!(
            plan_damage(&prev, &next, false),
            Damage::Region(rect(0, 0, 100, 20))
        );
    }

    #[test]
    fn a_layer_that_says_what_changed_inside_it_damages_only_that() {
        // The page (id 0) is where it was and reports one changed rectangle — a
        // field being typed into. The paragraphs around it keep their pixels.
        let prev = [
            (LayerId(0), 5, rect(0, 20, 100, 80)),
            (LayerId(1), 2, rect(0, 0, 100, 20)),
        ];
        let next = [
            partial(0, 6, rect(0, 20, 100, 80), rect(10, 30, 40, 10)),
            layer(1, 2, rect(0, 0, 100, 20)),
        ];
        assert_eq!(
            plan_damage(&prev, &next, false),
            Damage::Region(rect(10, 30, 40, 10))
        );
    }

    #[test]
    fn a_layer_that_moved_damages_where_it_was_whatever_it_says_changed() {
        // Its own account is about its contents; the pixels it left behind are
        // not its contents, and nothing else in the scene has drawn them yet.
        let prev = [(LayerId(0), 1, rect(0, 0, 10, 10))];
        let next = [partial(0, 2, rect(20, 0, 10, 10), rect(20, 0, 2, 2))];
        assert_eq!(
            plan_damage(&prev, &next, false),
            Damage::Region(rect(0, 0, 30, 10))
        );
    }

    #[test]
    fn a_reported_rectangle_is_cut_to_the_layer_it_belongs_to() {
        let prev = [(LayerId(0), 1, rect(0, 20, 100, 80))];
        let next = [partial(0, 2, rect(0, 20, 100, 80), rect(50, 0, 100, 200))];
        assert_eq!(
            plan_damage(&prev, &next, false),
            Damage::Region(rect(50, 20, 50, 80))
        );
    }

    #[test]
    fn a_changed_layer_with_offscreen_damage_redraws_nothing() {
        let prev = [(LayerId(0), 1, rect(0, 20, 100, 80))];
        let next = [partial(0, 2, rect(0, 20, 100, 80), rect(10, 0, 30, 10))];
        assert_eq!(plan_damage(&prev, &next, false), Damage::Unchanged);
    }

    #[test]
    fn a_moved_rectangle_damages_both_where_it_was_and_where_it_is() {
        let prev = [(LayerId(0), 1, rect(0, 0, 10, 10))];
        let next = [layer(0, 1, rect(20, 0, 10, 10))];
        assert_eq!(
            plan_damage(&prev, &next, false),
            Damage::Region(rect(0, 0, 30, 10))
        );
    }

    /// A painter of two coloured bands, so a test can say what the window should
    /// look like and then look at it.
    struct Panes {
        top: otlyra_gfx::peniko::Color,
        bottom: otlyra_gfx::peniko::Color,
        epoch: u64,
        frames: u64,
    }

    impl Panes {
        fn list(
            rect: LayerRect,
            color: otlyra_gfx::peniko::Color,
        ) -> std::sync::Arc<otlyra_gfx::DisplayList> {
            use otlyra_gfx::kurbo::Shape;

            let mut list = otlyra_gfx::DisplayList::new();
            list.push(otlyra_gfx::DisplayItem::Fill {
                style: otlyra_gfx::peniko::Fill::NonZero,
                transform: otlyra_gfx::kurbo::Affine::IDENTITY,
                brush: otlyra_gfx::peniko::Brush::Solid(color),
                brush_transform: None,
                shape: otlyra_gfx::kurbo::Rect::new(
                    f64::from(rect.x),
                    f64::from(rect.y),
                    f64::from(rect.x + rect.width),
                    f64::from(rect.y + rect.height),
                )
                .to_path(0.1),
            });
            std::sync::Arc::new(list)
        }
    }

    impl Painter for Panes {
        fn compose(&mut self, viewport: Viewport) -> Option<Scene> {
            self.frames += 1;
            let half = viewport.height / 2;
            let top = rect(0, 0, viewport.width, half);
            let bottom = rect(0, half, viewport.width, viewport.height - half);
            Some(Scene {
                layers: vec![
                    crate::SceneLayer {
                        id: LayerId(0),
                        rect: bottom,
                        epoch: 1,
                        list: Self::list(bottom, self.bottom),
                        dirty: None,
                    },
                    crate::SceneLayer {
                        id: LayerId(1),
                        rect: top,
                        epoch: self.epoch,
                        list: Self::list(top, self.top),
                        dirty: None,
                    },
                ],
            })
        }

        fn handle_event(&mut self, event: PlatformEvent) -> FrameRequest {
            // One event changes the top layer and nothing else, which is the
            // shape of every chrome-only interaction.
            if matches!(event, PlatformEvent::PointerPressed { .. }) {
                self.top = otlyra_gfx::peniko::Color::from_rgba8(0, 0, 255, 255);
                self.epoch += 1;
                return FrameRequest::Now;
            }
            FrameRequest::None
        }

        fn paint(&mut self, _target: &mut dyn PaintTarget, _viewport: Viewport) {
            unreachable!("this painter composes");
        }
    }

    fn pixel(pixels: &[u8], viewport: Viewport, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * viewport.width + x) * 4) as usize;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    }

    #[test]
    fn the_pump_hands_back_the_pixels_the_layers_drew() {
        let viewport = Viewport::new(8, 8, 1.0);
        let mut painter = Panes {
            top: otlyra_gfx::peniko::Color::from_rgba8(255, 0, 0, 255),
            bottom: otlyra_gfx::peniko::Color::from_rgba8(0, 255, 0, 255),
            epoch: 1,
            frames: 0,
        };
        let mut pump = FramePump::new(viewport);
        assert_eq!(pump.open(&mut painter).expect("a frame"), Damage::Full);

        let pixels = pump.pixels().expect("pixels");
        assert_eq!(pixel(&pixels, viewport, 4, 1), [255, 0, 0, 255]);
        assert_eq!(pixel(&pixels, viewport, 4, 6), [0, 255, 0, 255]);
    }

    #[test]
    fn an_event_that_moves_one_layer_damages_only_that_layer() {
        let viewport = Viewport::new(8, 8, 1.0);
        let mut painter = Panes {
            top: otlyra_gfx::peniko::Color::from_rgba8(255, 0, 0, 255),
            bottom: otlyra_gfx::peniko::Color::from_rgba8(0, 255, 0, 255),
            epoch: 1,
            frames: 0,
        };
        let mut pump = FramePump::new(viewport);
        pump.open(&mut painter).expect("a frame");

        pump.event(&mut painter, PlatformEvent::PointerPressed { clicks: 1 });
        assert!(pump.frame_requested());
        assert_eq!(pump.settle(&mut painter).expect("settled"), 1);

        // The damage is the top layer's rectangle, and the pixels really changed.
        assert_eq!(pump.damage(), Damage::Region(rect(0, 0, 8, 4)));
        let pixels = pump.pixels().expect("pixels");
        assert_eq!(pixel(&pixels, viewport, 4, 1), [0, 0, 255, 255]);
        assert_eq!(pixel(&pixels, viewport, 4, 6), [0, 255, 0, 255]);
    }

    #[test]
    fn an_event_nothing_reacts_to_draws_no_frame() {
        let viewport = Viewport::new(8, 8, 1.0);
        let mut painter = Panes {
            top: otlyra_gfx::peniko::Color::from_rgba8(255, 0, 0, 255),
            bottom: otlyra_gfx::peniko::Color::from_rgba8(0, 255, 0, 255),
            epoch: 1,
            frames: 0,
        };
        let mut pump = FramePump::new(viewport);
        pump.open(&mut painter).expect("a frame");
        let drawn = painter.frames;

        pump.event(&mut painter, PlatformEvent::PointerMoved { x: 1.0, y: 1.0 });
        assert!(!pump.frame_requested());
        assert_eq!(pump.settle(&mut painter).expect("settled"), 0);
        assert_eq!(painter.frames, drawn, "no frame was composed");
    }
}
