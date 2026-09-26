//! The retained-layer compositor, and a window's frame path with no window.
//!
//! One implementation, two callers. [`SceneCompositor`] owns the persistent
//! raster surface, works out what a frame has to redraw from the layers the
//! embedder published, and re-rasterizes only that; a window's render thread
//! hands the result to a swapchain, and [`FramePump`] hands it back to the caller
//! as pixels. That is the point of the split: a test or a driver that wants to know
//! what the window shows runs the *same* `compose` → damage → retained-surface
//! path the window runs, rather than a second whole-surface paint that can agree
//! with the tests and disagree with the screen.

use otlyra_gfx::{PaintTarget, SkiaPainter};

use crate::{FrameRequest, LayerId, LayerRect, Painter, PlatformEvent, Scene, Viewport};

/// What one composited frame had to redraw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Damage {
    /// Nothing changed; the retained frame stands.
    Unchanged,
    /// One region moved; it alone was re-rasterized and re-uploaded.
    Region(LayerRect),
    /// One layer's pixels were moved rather than drawn again, and only what the
    /// move could not account for was drawn.
    Scrolled(Scroll),
    /// The whole surface — first frame, a resize, or a structural change.
    Full,
}

/// A layer that moved as a whole: a scroll.
///
/// The layer kept its list and its place and changed only the offsets it is
/// drawn with, so most of its pixels are the ones already on the surface, a
/// whole number of pixels further along. Those are moved; what has to be drawn
/// is the strip the move uncovered and whatever in the layer moves differently
/// from the rest of it — a fixed header, a sticky heading, a scrollbar thumb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scroll {
    /// The layer whose pixels moved, in device pixels.
    pub rect: LayerRect,
    /// How far they moved, in device pixels.
    pub by: (i32, i32),
    /// What is drawn afresh afterwards.
    pub repaint: Vec<LayerRect>,
}

impl Damage {
    /// Device pixels this damage re-rasterized, for the rasterization counters.
    pub(crate) fn pixels(&self, viewport: Viewport) -> u64 {
        let area = |rect: &LayerRect| u64::from(rect.width) * u64::from(rect.height);
        match self {
            Damage::Unchanged => 0,
            Damage::Region(rect) => area(rect),
            Damage::Scrolled(scroll) => scroll.repaint.iter().map(area).sum(),
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

/// One layer as it was last put on the surface: enough to tell next frame what
/// changed about it, including when all that changed is where it is scrolled to.
struct Retained {
    id: LayerId,
    epoch: u64,
    rect: LayerRect,
    list: std::sync::Arc<otlyra_gfx::DisplayList>,
    offsets: Vec<otlyra_gfx::kurbo::Vec2>,
}

/// The persistent raster surface and the layers that are currently on it.
///
/// A layer whose epoch and rectangle are unchanged keeps the pixels it already
/// has, so a frame that touches only the toolbar neither re-rasterizes nor
/// re-uploads the page underneath it.
pub(crate) struct SceneCompositor {
    /// The retained surface. `None` until the first frame sizes it.
    surface: Option<SkiaPainter>,
    /// Last presented frame's layers, in order.
    prev: Vec<Retained>,
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
                    otlyra_gfx::render_scrolled(&layer.list, surface, &layer.offsets);
                }
                Upload::Full(surface.read_rgba8().map_err(Box::new)?)
            }
            Damage::Region(rect) => {
                redraw(surface, scene, rect);
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
            Damage::Scrolled(ref scroll) => {
                apply_scroll(surface, scene, scroll);
                // Every pixel of the moved layer is new to the screen, and so is
                // anything drawn outside it for another layer.
                let rect = scroll
                    .repaint
                    .iter()
                    .fold(scroll.rect, |union, rect| union.union(rect));
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
            .map(|layer| Retained {
                id: layer.id,
                epoch: layer.epoch,
                rect: layer.rect,
                list: std::sync::Arc::clone(&layer.list),
                offsets: layer.offsets.clone(),
            })
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
/// longer be trusted position-for-position. A layer that kept its list and its
/// place and changed only its offsets has scrolled, and its pixels are moved
/// rather than drawn (see [`Scroll`]). Otherwise the damage is the union of the
/// old and new rectangles of every layer whose epoch or rectangle moved.
fn plan_damage(prev: &[Retained], next: &[crate::SceneLayer], forced_full: bool) -> Damage {
    if forced_full || prev.len() != next.len() {
        return Damage::Full;
    }
    let mut damage: Option<LayerRect> = None;
    let mut scroll: Option<Scroll> = None;
    for (previous, layer) in prev.iter().zip(next.iter()) {
        if previous.id != layer.id {
            return Damage::Full;
        }
        if previous.epoch == layer.epoch && previous.rect == layer.rect {
            continue;
        }
        if scroll.is_none()
            && previous.rect == layer.rect
            && std::sync::Arc::ptr_eq(&previous.list, &layer.list)
            && let Some(planned) =
                plan_scroll(&layer.list, &previous.offsets, &layer.offsets, layer.rect)
        {
            scroll = Some(planned);
            continue;
        }
        // A layer that stayed where it is and says which part of itself
        // changed is taken at its word; one that moved has to answer for
        // where it was as well, and its own account of what changed inside it
        // says nothing about the pixels it has left behind.
        let moved = match layer.dirty {
            Some(dirty) if previous.rect == layer.rect => clamp(dirty, layer.rect),
            _ => previous.rect.union(&layer.rect),
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
    match (scroll, damage) {
        (None, None) => Damage::Unchanged,
        (None, Some(rect)) => Damage::Region(rect),
        (Some(mut scroll), extra) => {
            // Another layer over the scrolled one would have its pixels moved
            // along with it; then the layer is simply drawn again.
            let covered = next
                .iter()
                .filter(|layer| layer.rect != scroll.rect)
                .any(|layer| layer.rect.intersects(&scroll.rect));
            if covered {
                let rect = extra.map_or(scroll.rect, |extra| extra.union(&scroll.rect));
                return Damage::Region(rect);
            }
            scroll.repaint.extend(extra);
            Damage::Scrolled(scroll)
        }
    }
}

/// The scroll that takes `list` drawn with `before` to `list` drawn with
/// `after`, in the layer at `rect` — or `None` when moving pixels is not the
/// cheaper way there: the move is not a whole number of pixels, or so much has
/// to be drawn afterwards that drawing everything is as cheap.
///
/// The part of the list that covers the most, through its clips, is taken to be
/// what scrolled — the page; its move is the move. Every other part that moved differently — or did not move
/// at all, like a fixed header — is drawn again both where the move put its old
/// pixels and where it is now.
fn plan_scroll(
    list: &otlyra_gfx::DisplayList,
    before: &[otlyra_gfx::kurbo::Vec2],
    after: &[otlyra_gfx::kurbo::Vec2],
    rect: LayerRect,
) -> Option<Scroll> {
    // Where each part is, cut to what the clips let through: the contents of a
    // panel reach as far as there is content, but only the panel's rectangle of
    // them is on the screen.
    let (was_at, is_at) = (
        list.spatial_bounds_at(before),
        list.spatial_bounds_at(after),
    );
    let bulk = is_at
        .iter()
        .max_by(|(_, _, one), (_, _, other)| one.area().total_cmp(&other.area()))?
        .0;
    let moved = |spatial| {
        list.shift(spatial, after).translation() - list.shift(spatial, before).translation()
    };
    let by = moved(bulk);
    let whole = |value: f64| (value - value.round()).abs() < 1e-3;
    if !whole(by.x) || !whole(by.y) {
        return None;
    }
    let by_pixels = (by.x.round() as i32, by.y.round() as i32);
    if by_pixels.0.unsigned_abs() >= rect.width || by_pixels.1.unsigned_abs() >= rect.height {
        return None;
    }

    let mut repaint = Vec::new();
    // The strip the move uncovered.
    let (dx, dy) = (by_pixels.0.unsigned_abs(), by_pixels.1.unsigned_abs());
    if by_pixels.1 < 0 {
        repaint.push(LayerRect {
            y: rect.y + rect.height - dy,
            height: dy,
            ..rect
        });
    } else if by_pixels.1 > 0 {
        repaint.push(LayerRect { height: dy, ..rect });
    }
    if by_pixels.0 < 0 {
        repaint.push(LayerRect {
            x: rect.x + rect.width - dx,
            width: dx,
            ..rect
        });
    } else if by_pixels.0 > 0 {
        repaint.push(LayerRect { width: dx, ..rect });
    }
    // Whatever moved otherwise: where the move took its old pixels, and where it
    // is now — each cut to the clips it is drawn in, so that a panel's contents
    // are the panel's rectangle and not the length of what scrolls in it.
    // Every part that is on the screen now or was — one scrolled wholly out of
    // sight still left its pixels behind.
    let mut spatials: Vec<otlyra_gfx::SpatialId> =
        was_at.iter().chain(&is_at).map(|(id, _, _)| *id).collect();
    spatials.sort_unstable();
    spatials.dedup();
    for spatial in &spatials {
        let own = moved(*spatial);
        if *spatial == bulk || (own - by).hypot() < 1e-3 {
            continue;
        }
        let at = |bounds: &[(otlyra_gfx::SpatialId, usize, otlyra_gfx::kurbo::Rect)]| {
            bounds
                .iter()
                .find(|(id, _, _)| id == spatial)
                .map(|(_, _, rect)| *rect)
        };
        let areas = [at(&was_at).map(|was| was + by), at(&is_at)];
        for area in areas.into_iter().flatten() {
            let covered = clamp(pixel_rect(area), rect);
            if covered.width > 0 && covered.height > 0 {
                repaint.push(covered);
            }
        }
    }

    let repaint = merged(repaint);
    let area = |rect: &LayerRect| u64::from(rect.width) * u64::from(rect.height);
    let drawn: u64 = repaint.iter().map(area).sum();
    if drawn * 5 > area(&rect) * 3 {
        return None;
    }
    Some(Scroll {
        rect,
        by: by_pixels,
        repaint,
    })
}

/// `rects` with every two that overlap enough replaced by the rectangle around
/// both.
///
/// Something fixed to the screen is drawn again where it is and where the move
/// took its old pixels, and the two are the same rectangle a scroll step apart:
/// drawn as one, the overlap is drawn once. Two that merely touch are left
/// apart — the rectangle around a sliver down the left edge and a band across
/// the page is most of the page, and drawing that is what this is for avoiding.
fn merged(mut rects: Vec<LayerRect>) -> Vec<LayerRect> {
    let area = |rect: &LayerRect| u64::from(rect.width) * u64::from(rect.height);
    let mut changed = true;
    while changed {
        changed = false;
        'pairs: for one in 0..rects.len() {
            for other in one + 1..rects.len() {
                let around = rects[one].union(&rects[other]);
                if rects[one].intersects(&rects[other])
                    && area(&around) <= area(&rects[one]) + area(&rects[other])
                {
                    let absorbed = rects.swap_remove(other);
                    rects[one] = rects[one].union(&absorbed);
                    changed = true;
                    break 'pairs;
                }
            }
        }
    }
    rects
}

/// The whole pixels `area` touches, and one more on every side for antialiasing.
fn pixel_rect(area: otlyra_gfx::kurbo::Rect) -> LayerRect {
    let x0 = (area.x0.floor() - 1.0).max(0.0);
    let y0 = (area.y0.floor() - 1.0).max(0.0);
    let x1 = (area.x1.ceil() + 1.0).max(x0);
    let y1 = (area.y1.ceil() + 1.0).max(y0);
    LayerRect {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    }
}

/// Draw `rect` of the surface afresh from every layer that reaches it.
fn redraw(surface: &mut SkiaPainter, scene: &Scene, rect: LayerRect) {
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
            otlyra_gfx::render_scrolled(&layer.list, surface, &layer.offsets);
        }
    }
    surface.reset_clip();
}

/// Move a scrolled layer's pixels and draw what the move could not account for.
pub(crate) fn apply_scroll(surface: &mut SkiaPainter, scene: &Scene, scroll: &Scroll) {
    let rect = scroll.rect;
    surface.shift_rect(
        otlyra_gfx::kurbo::Rect::new(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.x + rect.width),
            f64::from(rect.y + rect.height),
        ),
        scroll.by,
    );
    for area in &scroll.repaint {
        redraw(surface, scene, *area);
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
        self.damage.clone()
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
        self.damage = damage.clone();
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
            offsets: Vec::new(),
            dirty: None,
        }
    }

    /// A layer as the previous frame left it.
    fn kept(id: u64, epoch: u64, rect: LayerRect) -> Retained {
        Retained {
            id: LayerId(id),
            epoch,
            rect,
            list: std::sync::Arc::new(otlyra_gfx::DisplayList::new()),
            offsets: Vec::new(),
        }
    }

    /// A page: forty bands down it that scroll, and a header fixed to the top
    /// of the layer.
    fn scrolling_page() -> std::sync::Arc<otlyra_gfx::DisplayList> {
        use otlyra_gfx::kurbo::Shape;
        let fill = |y: f64, height: f64, shade: u8| otlyra_gfx::DisplayItem::Fill {
            style: otlyra_gfx::peniko::Fill::NonZero,
            transform: otlyra_gfx::kurbo::Affine::IDENTITY,
            brush: otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                shade,
                255 - shade,
                128,
            )),
            brush_transform: None,
            shape: otlyra_gfx::kurbo::Rect::new(0.0, y, 100.0, y + height).to_path(0.1),
        };
        let mut list = otlyra_gfx::DisplayList::new();
        list.set_spatial(otlyra_gfx::SpatialId(1));
        for band in 0..40_u8 {
            list.push(fill(f64::from(band) * 10.0, 10.0, band * 6));
        }
        list.set_spatial(otlyra_gfx::SpatialId::VIEWPORT);
        list.push(fill(0.0, 12.0, 250));
        std::sync::Arc::new(list)
    }

    fn scrolled(y: f64) -> Vec<otlyra_gfx::kurbo::Vec2> {
        vec![
            otlyra_gfx::kurbo::Vec2::ZERO,
            otlyra_gfx::kurbo::Vec2::new(0.0, y),
        ]
    }

    #[test]
    fn a_scroll_moves_the_pixels_and_draws_the_strip_and_what_did_not_move() {
        let list = scrolling_page();
        let area = rect(0, 0, 100, 200);
        let prev = [Retained {
            offsets: scrolled(0.0),
            list: std::sync::Arc::clone(&list),
            ..kept(0, 1, area)
        }];
        let next = [crate::SceneLayer {
            offsets: scrolled(40.0),
            list,
            ..layer(0, 2, area)
        }];
        let Damage::Scrolled(scroll) = plan_damage(&prev, &next, false) else {
            panic!("a scroll is moved, not drawn");
        };
        assert_eq!(scroll.by, (0, -40));
        assert!(
            scroll.repaint.contains(&rect(0, 160, 100, 40)),
            "the strip uncovered"
        );
        assert!(
            scroll
                .repaint
                .iter()
                .any(|repaint| repaint.y == 0 && repaint.height >= 12),
            "the header, which did not move with the page"
        );
    }

    #[test]
    fn rectangles_are_merged_only_where_merging_draws_no_more() {
        // A header drawn where it is and where the scroll took its old pixels.
        let header = merged(vec![rect(0, 0, 100, 12), rect(0, 0, 100, 16)]);
        assert_eq!(header, vec![rect(0, 0, 100, 16)]);
        // A sliver down the left edge and a band across the page touch, and the
        // rectangle around them would be most of the page.
        let apart = merged(vec![rect(0, 0, 1, 180), rect(0, 60, 100, 20)]);
        assert_eq!(apart.len(), 2);
    }

    /// A page with a panel that scrolls: the panel's box and its edge move
    /// with the page, and forty bands inside it move with the panel.
    fn page_with_a_panel() -> std::sync::Arc<otlyra_gfx::DisplayList> {
        use otlyra_gfx::kurbo::Shape;
        let fill = |x: f64, y: f64, height: f64, shade: u8| otlyra_gfx::DisplayItem::Fill {
            style: otlyra_gfx::peniko::Fill::NonZero,
            transform: otlyra_gfx::kurbo::Affine::IDENTITY,
            brush: otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                shade,
                128,
                255 - shade,
            )),
            brush_transform: None,
            shape: otlyra_gfx::kurbo::Rect::new(x, y, x + 40.0, y + height).to_path(0.1),
        };
        let mut list = otlyra_gfx::DisplayList::new();
        list.set_spatial(otlyra_gfx::SpatialId(1));
        list.push(fill(0.0, 0.0, 400.0, 10));
        list.push(otlyra_gfx::DisplayItem::PushLayer {
            blend: otlyra_gfx::peniko::BlendMode::default(),
            alpha: 1.0,
            transform: otlyra_gfx::kurbo::Affine::IDENTITY,
            clip: otlyra_gfx::kurbo::Rect::new(50.0, 20.0, 90.0, 80.0).to_path(0.1),
        });
        list.set_spatial(otlyra_gfx::SpatialId(2));
        for band in 0..40_u8 {
            list.push(fill(50.0, 20.0 + f64::from(band) * 10.0, 10.0, band * 6));
        }
        list.set_spatial(otlyra_gfx::SpatialId(1));
        list.push(otlyra_gfx::DisplayItem::PopLayer);
        std::sync::Arc::new(list)
    }

    #[test]
    fn a_panel_that_scrolls_draws_its_own_rectangle_and_nothing_else() {
        let list = page_with_a_panel();
        let area = rect(0, 0, 100, 200);
        let at = |panel: f64| {
            vec![
                otlyra_gfx::kurbo::Vec2::ZERO,
                otlyra_gfx::kurbo::Vec2::ZERO,
                otlyra_gfx::kurbo::Vec2::new(0.0, panel),
            ]
        };
        let scene = |panel: f64, epoch: u64| crate::Scene {
            layers: vec![crate::SceneLayer {
                offsets: at(panel),
                list: std::sync::Arc::clone(&list),
                ..layer(0, epoch, area)
            }],
        };
        let viewport = Viewport::new(100, 200, 1.0);
        let mut moved = SceneCompositor::new();
        moved.rasterize(&scene(0.0, 1), viewport).expect("a frame");
        moved.commit(&scene(0.0, 1));

        let frame = scene(30.0, 2);
        let composited = moved.rasterize(&frame, viewport).expect("a frame");
        let Damage::Scrolled(scroll) = &composited.damage else {
            panic!("a panel's scroll is not a redraw of the page");
        };
        assert_eq!(scroll.by, (0, 0), "the page did not move");
        let drawn: u32 = scroll
            .repaint
            .iter()
            .map(|rect| rect.width * rect.height)
            .sum();
        assert!(drawn <= 44 * 64, "drew {drawn} pixels for a 40 by 60 panel");
        moved.commit(&frame);

        let mut whole = SceneCompositor::new();
        whole.rasterize(&frame, viewport).expect("a frame");
        assert!(moved.read_rgba8().expect("pixels") == whole.read_rgba8().expect("pixels"));
    }

    #[test]
    fn a_scroll_by_a_fraction_of_a_pixel_is_drawn() {
        let list = scrolling_page();
        let area = rect(0, 0, 100, 200);
        let prev = [Retained {
            offsets: scrolled(0.0),
            list: std::sync::Arc::clone(&list),
            ..kept(0, 1, area)
        }];
        let next = [crate::SceneLayer {
            offsets: scrolled(10.5),
            list,
            ..layer(0, 2, area)
        }];
        assert_eq!(plan_damage(&prev, &next, false), Damage::Region(area));
    }

    #[test]
    fn a_moved_frame_is_the_frame_drawn_whole() {
        let list = scrolling_page();
        let area = rect(0, 0, 100, 200);
        let scene = |y: f64, epoch: u64| crate::Scene {
            layers: vec![crate::SceneLayer {
                offsets: scrolled(y),
                list: std::sync::Arc::clone(&list),
                ..layer(0, epoch, area)
            }],
        };
        let viewport = Viewport::new(100, 200, 1.0);

        let mut moved = SceneCompositor::new();
        moved.rasterize(&scene(0.0, 1), viewport).expect("a frame");
        moved.commit(&scene(0.0, 1));
        for (step, y) in [37.0, 80.0, 45.0].into_iter().enumerate() {
            let frame = scene(y, step as u64 + 2);
            let composited = moved.rasterize(&frame, viewport).expect("a frame");
            assert!(matches!(composited.damage, Damage::Scrolled(_)));
            moved.commit(&frame);

            let mut whole = SceneCompositor::new();
            whole.rasterize(&frame, viewport).expect("a frame");
            assert!(
                moved.read_rgba8().expect("pixels") == whole.read_rgba8().expect("pixels"),
                "scrolled to {y}, the moved frame differs from the one drawn whole"
            );
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
        let prev = [kept(0, 1, rect(0, 0, 10, 10))];
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
            kept(0, 5, rect(0, 20, 100, 80)),
            kept(1, 2, rect(0, 0, 100, 20)),
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
            kept(0, 5, rect(0, 20, 100, 80)),
            kept(1, 2, rect(0, 0, 100, 20)),
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
            kept(0, 5, rect(0, 20, 100, 80)),
            kept(1, 2, rect(0, 0, 100, 20)),
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
        let prev = [kept(0, 1, rect(0, 0, 10, 10))];
        let next = [partial(0, 2, rect(20, 0, 10, 10), rect(20, 0, 2, 2))];
        assert_eq!(
            plan_damage(&prev, &next, false),
            Damage::Region(rect(0, 0, 30, 10))
        );
    }

    #[test]
    fn a_reported_rectangle_is_cut_to_the_layer_it_belongs_to() {
        let prev = [kept(0, 1, rect(0, 20, 100, 80))];
        let next = [partial(0, 2, rect(0, 20, 100, 80), rect(50, 0, 100, 200))];
        assert_eq!(
            plan_damage(&prev, &next, false),
            Damage::Region(rect(50, 20, 50, 80))
        );
    }

    #[test]
    fn a_changed_layer_with_offscreen_damage_redraws_nothing() {
        let prev = [kept(0, 1, rect(0, 20, 100, 80))];
        let next = [partial(0, 2, rect(0, 20, 100, 80), rect(10, 0, 30, 10))];
        assert_eq!(plan_damage(&prev, &next, false), Damage::Unchanged);
    }

    #[test]
    fn a_moved_rectangle_damages_both_where_it_was_and_where_it_is() {
        let prev = [kept(0, 1, rect(0, 0, 10, 10))];
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
                        offsets: Vec::new(),
                        dirty: None,
                    },
                    crate::SceneLayer {
                        id: LayerId(1),
                        rect: top,
                        epoch: self.epoch,
                        list: Self::list(top, self.top),
                        offsets: Vec::new(),
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
