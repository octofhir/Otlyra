//! # otlyra-paint — fragments to a display list
//!
//! ## Purpose
//!
//! The last step before pixels, and a pure function: a laid-out page in, a flat
//! list of drawing commands out. Nothing here allocates a GPU resource, touches a
//! rasterizer or knows which one is installed.
//!
//! ## Contents
//!
//! - [`build_display_list`] — the whole crate.
//!
//! ## Invariants
//!
//! 1. **Pure.** The same fragment tree and viewport always produce the same list,
//!    which is what makes display-list snapshots a regression test rather than a
//!    record of one machine's mood.
//! 2. **Paint order is document order.** Backgrounds, then text, walking the tree
//!    depth first. Stacking contexts and `z-index` arrive with `position`.
//! 3. **Off-screen fragments produce no items at all.** Culling here is cheaper
//!    than clipping in the rasterizer, and on a long page it removes most of the
//!    page.

use otlyra_gfx::kurbo::{Affine, BezPath, Rect as KurboRect, Shape};
use otlyra_gfx::peniko::{BlendMode, Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList, HitTestId};
use otlyra_layout::fragment::{Fragment, FragmentKind, FragmentTree, Rect};

/// Flattening tolerance for shapes entering the display list. Matches the recording
/// backend's, so a display list and its recording agree.
const PATH_TOLERANCE: f64 = 0.1;

/// How wide a scrollbar is drawn, in logical pixels.
const SCROLLBAR_WIDTH: f32 = 8.0;

/// How far a scrollbar sits from the edge it runs along.
const SCROLLBAR_INSET: f32 = 2.0;

/// The shortest a scrollbar's thumb is drawn, so a very long page still has
/// something to see and to aim at.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// The scrollbar's thumb.
const SCROLLBAR_THUMB: Color = Color::from_rgba8(0, 0, 0, 0x59);

/// Where a scrollbar's thumb is, for an area showing `content_height` scrolled to
/// `scroll` — or `None` when the content fits and there is no scrollbar.
///
/// One function, used to draw it and to decide what a press landed on: a scrollbar
/// that is drawn in one place and grabbed in another is the same bug as a link that
/// is clickable somewhere else.
pub fn scrollbar_thumb(area: Rect, content_height: f32, scroll: f32) -> Option<Rect> {
    let range = content_height - area.height;
    if range <= 0.5 || area.height <= 0.0 {
        return None;
    }

    let visible = (area.height / content_height).clamp(0.0, 1.0);
    let thumb = (area.height * visible).max(SCROLLBAR_MIN_THUMB.min(area.height));
    let travel = area.height - thumb;
    let at = area.y + travel * (scroll / range).clamp(0.0, 1.0);
    Some(Rect::new(
        area.right() - SCROLLBAR_WIDTH - SCROLLBAR_INSET,
        at,
        SCROLLBAR_WIDTH,
        thumb,
    ))
}

/// How far a scrollbar's thumb travels: the pixels of thumb movement that stand for
/// the whole of the content.
pub fn scrollbar_travel(area: Rect, content_height: f32) -> f32 {
    match scrollbar_thumb(area, content_height, 0.0) {
        Some(thumb) => (area.height - thumb.height).max(0.0),
        None => 0.0,
    }
}

/// Draw a scrollbar down the right edge of `area`.
///
/// Nothing is drawn for content that fits: a scrollbar that says the page cannot
/// move is noise. The thumb's length is the fraction of the content on screen and
/// its position is how far through the content that fraction is, which is the whole
/// of what a scrollbar says.
fn paint_scrollbar(list: &mut DisplayList, area: Rect, content_height: f32, scroll: f32) {
    let Some(thumb) = scrollbar_thumb(area, content_height, scroll) else {
        return;
    };

    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(SCROLLBAR_THUMB),
        brush_transform: None,
        shape: otlyra_gfx::kurbo::RoundedRect::new(
            f64::from(thumb.x),
            f64::from(thumb.y),
            f64::from(thumb.right()),
            f64::from(thumb.bottom()),
            f64::from(SCROLLBAR_WIDTH / 2.0),
        )
        .to_path(PATH_TOLERANCE),
    });
}

/// The colour a selection is drawn in.
///
/// The platform's own highlight is a preference this cannot read yet; this is the
/// blue every browser falls back to, and it is opaque because the text is drawn
/// over it rather than through it.
const SELECTION: Color = Color::from_rgb8(0xB4, 0xD5, 0xFE);

/// A box that draws its contents as a group: composited once, moved as one, or
/// both.
struct Group<'a> {
    fragment: &'a Fragment,
    /// Whether a compositing layer was opened for it, which has to be closed.
    layer: bool,
    /// What to move everything it drew by, if it is transformed.
    transform: Option<Affine>,
    /// The first item drawn inside it.
    from: usize,
}

/// Finish a group: move what it drew, then close the layer it opened.
///
/// In that order. The layer is a compositing step around the drawing, and the
/// transform is a property of the drawing itself — applied after the layer was
/// closed it would move the boundary rather than the contents.
fn close(group: Group<'_>, list: &mut DisplayList) {
    if let Some(transform) = group.transform {
        list.transform_from(group.from, transform);
    }
    if group.layer {
        list.push(DisplayItem::PopLayer);
    }
}

/// The matrix a box's `transform` comes to, about its own origin.
///
/// `None` when the box is not transformed, which is nearly every box on nearly
/// every page. The origin is a point in the box's *border* box — the middle of it
/// unless `transform-origin` says otherwise — so the steps are applied there and
/// the box put back afterwards, which is what makes `rotate()` turn a card about
/// its middle rather than swing it about the corner of the page.
fn transform_of(fragment: &Fragment, scroll_y: f32) -> Option<Affine> {
    let steps = &fragment.style.transform;
    if steps.is_empty() {
        return None;
    }

    let rect = fragment.rect;
    let mut matrix = Affine::IDENTITY;
    for step in steps.iter() {
        matrix *= match *step {
            otlyra_css::TransformOp::Translate(x, y) => Affine::translate((
                f64::from(x.resolve(rect.width)),
                f64::from(y.resolve(rect.height)),
            )),
            otlyra_css::TransformOp::Scale(x, y) => Affine::scale_non_uniform(x.into(), y.into()),
            otlyra_css::TransformOp::Rotate(radians) => Affine::rotate(radians.into()),
            otlyra_css::TransformOp::Skew(x, y) => {
                Affine::new([1.0, f64::from(y).tan(), f64::from(x).tan(), 1.0, 0.0, 0.0])
            }
            otlyra_css::TransformOp::Matrix([a, b, c, d, e, f]) => {
                Affine::new([a.into(), b.into(), c.into(), d.into(), e.into(), f.into()])
            }
        };
    }

    let origin = (
        f64::from(rect.x + fragment.style.transform_origin.x.resolve(rect.width)),
        f64::from(rect.y - scroll_y + fragment.style.transform_origin.y.resolve(rect.height)),
    );
    Some(Affine::translate(origin) * matrix * Affine::translate((-origin.0, -origin.1)))
}

/// Build the display list for `tree`, showing the part of the page under
/// `scroll_y`, at `viewport` logical size.
pub fn build_display_list(tree: &FragmentTree, viewport: (f32, f32), scroll_y: f32) -> DisplayList {
    build_display_list_with(
        tree,
        &Frame {
            viewport,
            scroll_y,
            ..Frame::default()
        },
    )
}

/// Where a background picture is found, by the address a style names.
pub type BackgroundLookup<'a> = &'a dyn Fn(&str) -> Option<otlyra_gfx::peniko::ImageData>;

/// How far a scroll port has been scrolled.
pub type PortOffset<'a> = &'a dyn Fn(otlyra_layout::BoxId) -> f32;

/// Everything a frame needs beyond the fragments themselves.
///
/// A struct rather than four more arguments: what a frame is made of grows, and a
/// caller that wants only the scroll offset should not have to know about the rest.
pub struct Frame<'a> {
    /// The size of the visible area, in logical pixels.
    pub viewport: (f32, f32),
    /// How far down the page the reader is.
    pub scroll_y: f32,
    /// How far each scroll port has been scrolled.
    pub port_offset: Option<PortOffset<'a>>,
    /// The decoded picture behind a box, by the address its style names.
    pub background: Option<BackgroundLookup<'a>>,
    /// Whether scrollbars are drawn. Off for a picture that is going to be compared
    /// with one from elsewhere: a scrollbar is the browser's, not the page's.
    pub scrollbars: bool,
    /// What the reader has selected, in page coordinates.
    ///
    /// Drawn behind the text rather than over it, which is what makes the letters
    /// still readable: a highlight over them would tint them, and inverting them
    /// instead is a different tradition that this platform is not in.
    pub selection: &'a [Rect],
}

impl Default for Frame<'_> {
    fn default() -> Self {
        Self {
            viewport: (0.0, 0.0),
            scroll_y: 0.0,
            port_offset: None,
            background: None,
            scrollbars: true,
            selection: &[],
        }
    }
}

/// Build the display list, with each scroll port at the offset the caller says.
///
/// The page's own scroll is one argument and a box's is another because they are
/// different things: the page moves everything, and a scroll port moves only what
/// is inside it — while the box itself, and the edge it cuts its contents off at,
/// stay where they are.
pub fn build_display_list_scrolled(
    tree: &FragmentTree,
    viewport: (f32, f32),
    scroll_y: f32,
    port_offset: &dyn Fn(otlyra_layout::BoxId) -> f32,
) -> DisplayList {
    build_display_list_with(
        tree,
        &Frame {
            viewport,
            scroll_y,
            port_offset: Some(port_offset),
            ..Frame::default()
        },
    )
}

/// Build the display list for one frame.
pub fn build_display_list_with(tree: &FragmentTree, frame: &Frame<'_>) -> DisplayList {
    let (viewport, scroll_y) = (frame.viewport, frame.scroll_y);
    let port_offset = |id| frame.port_offset.map_or(0.0, |lookup| lookup(id));
    let _span = tracing::info_span!("build_display_list").entered();
    let (width, height) = viewport;
    let mut list = DisplayList::new();

    // The canvas. CSS gives the root element's background to the canvas rather than
    // to the element: it covers the whole viewport however short the document is,
    // and the element does not paint it a second time — which is what lets a box
    // with a negative `z-index` show through from under the flow.
    // And when the root element has none of its own, the body's goes to the canvas
    // instead — which is why a page that colours only its body still has that
    // colour behind its margins, and out past the end of its content.
    let root_element = tree.root.children.first();
    let body = root_element.and_then(|root| {
        root.children
            .iter()
            .find(|child| matches!(child.kind, FragmentKind::Box))
    });
    let opaque = |fragment: &&Fragment| fragment.style.background_color.components[3] > 0.0;
    let canvas_from = root_element.filter(opaque).or_else(|| body.filter(opaque));
    // White when nobody says otherwise: the canvas is not an element and has no
    // style of its own, and a page that names no background is drawn on white. The
    // user-agent sheet deliberately does not put one on `html` — a background there
    // would be painted rather than propagated, and the body's would have nowhere
    // to go.
    let canvas = canvas_from
        .map(|fragment| fragment.style.background_color)
        .filter(|colour| colour.components[3] > 0.0)
        .unwrap_or(Color::WHITE);
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(canvas),
        brush_transform: None,
        shape: KurboRect::new(0.0, 0.0, f64::from(width), f64::from(height))
            .to_path(PATH_TOLERANCE),
    });

    let scrolled = Rect::new(0.0, scroll_y, width, height);
    let screen = Rect::new(0.0, 0.0, width, height);

    // Painting order is the tree's, and then the layers': everything in the flow,
    // then whatever a `position` and a `z-index` lifted above it or pushed below.
    // A stable sort, so that within one layer document order still decides.
    let mut visible: Vec<&Fragment> = tree.visible(&scrolled, &screen).collect();
    visible.sort_by(|one, other| one.layer.cmp(&other.layer));

    // The groups a box opens over its own contents. A half-transparent element and
    // everything in it is composited once, as a group: applied to each box on its
    // own instead, a box over another inside it would show the one underneath
    // through it, and the two of them together would be darker than either. A
    // transformed element is the same shape of thing — what is inside it is drawn
    // in its space — and is applied to the items rather than through a layer, so
    // that hit testing, which already undoes an item's transform, follows without
    // being told.
    let mut groups: Vec<Group<'_>> = Vec::new();

    for fragment in visible {
        while groups
            .last()
            .is_some_and(|open| !open.fragment.layer.contains(&fragment.layer))
        {
            close(groups.pop().expect("just looked"), &mut list);
        }
        if matches!(fragment.kind, FragmentKind::Box) {
            let faded = fragment.style.opacity < 1.0;
            let moved = transform_of(fragment, scroll_y);
            if faded || moved.is_some() {
                if faded {
                    list.push(DisplayItem::PushLayer {
                        blend: BlendMode::default(),
                        alpha: fragment.style.opacity,
                        transform: Affine::IDENTITY,
                        // The whole viewport: a group is a compositing step, not a
                        // clip, and a box whose contents reach outside it still
                        // shows them.
                        clip: KurboRect::new(0.0, 0.0, f64::from(width), f64::from(height))
                            .to_path(PATH_TOLERANCE),
                    });
                }
                groups.push(Group {
                    fragment,
                    layer: faded,
                    transform: moved,
                    from: list.len(),
                });
            }
        }

        // The initial containing block was painted as the canvas above; painting it
        // again would put a second full-viewport fill in every frame.
        if std::ptr::eq(fragment, &tree.root) {
            continue;
        }
        // Whichever background went to the canvas is not painted again by the box
        // it came from.
        let is_root_element = canvas_from.is_some_and(|from| std::ptr::eq(fragment, from))
            || root_element.is_some_and(|root| std::ptr::eq(fragment, root));
        let inside = fragment.scroll_port.map_or(0.0, &port_offset);
        // The highlight goes under the run it covers, so the letters are drawn over
        // it rather than through it.
        if matches!(fragment.kind, FragmentKind::Text(_)) {
            for rect in frame.selection {
                let covered = rect.intersection(&fragment.rect);
                if covered.width <= 0.0 || covered.height <= 0.0 {
                    continue;
                }
                let moved = if fragment.fixed {
                    covered
                } else {
                    Rect::new(
                        covered.x,
                        covered.y - scroll_y - inside,
                        covered.width,
                        covered.height,
                    )
                };
                list.push(DisplayItem::Fill {
                    style: Fill::NonZero,
                    transform: Affine::IDENTITY,
                    brush: Brush::Solid(SELECTION),
                    brush_transform: None,
                    shape: KurboRect::new(
                        f64::from(moved.x),
                        f64::from(moved.y),
                        f64::from(moved.right()),
                        f64::from(moved.bottom()),
                    )
                    .to_path(PATH_TOLERANCE),
                });
            }
        }
        paint(
            fragment,
            scroll_y + inside,
            scroll_y,
            height,
            is_root_element,
            frame.background,
            &mut list,
        );
    }

    while let Some(group) = groups.pop() {
        close(group, &mut list);
    }

    // Scrollbars last, over everything: the page's, and one for each port that has
    // more to show than it can.
    if !frame.scrollbars {
        tracing::debug!(items = list.len(), "display list built");
        return list;
    }
    paint_scrollbar(
        &mut list,
        Rect::new(0.0, 0.0, width, height),
        tree.content_height(),
        scroll_y,
    );
    for port in &tree.scroll_ports {
        let offset = port_offset(port.id);
        let mut area = port.port;
        area.y -= scroll_y;
        if area.bottom() < 0.0 || area.y > height {
            continue;
        }
        paint_scrollbar(&mut list, area, port.content_height, offset);
    }

    tracing::debug!(items = list.len(), "display list built");
    list
}

/// One fragment's own drawing. Children are visited by the caller's walk, so this
/// never recurses — a fragment whose parent was culled may still be visible.
/// `scroll_y` moves this fragment; `page_scroll` moves the page. They differ by
/// however far the scroll port this fragment is inside has been scrolled — and the
/// edge that port cuts its contents off at moves with the page, not with them.
#[allow(clippy::too_many_arguments)]
fn paint(
    fragment: &Fragment,
    scroll_y: f32,
    page_scroll: f32,
    viewport_height: f32,
    background_on_canvas: bool,
    background_picture: Option<BackgroundLookup<'_>>,
    list: &mut DisplayList,
) {
    // A sticky box moves with the page until the scroll would take it past its
    // inset, and then holds there until its container runs out from under it.
    let rect = match fragment.sticky {
        Some(sticky) => {
            let mut rect = fragment.rect;
            rect.y += sticky_shift(sticky, scroll_y, viewport_height);
            rect
        }
        None => fragment.rect,
    };
    // A fixed fragment is already in screen coordinates: it stays where it is
    // however far the page has been scrolled.
    let scroll_y = if fragment.fixed { 0.0 } else { scroll_y };

    // Whatever an ancestor cuts this fragment off at, as a layer around its own
    // drawing. One layer per fragment rather than one around a subtree, because the
    // walk is flat: a fragment carries the rectangle it is cut off at, so the two
    // cannot disagree about where the edge is.
    // The clip belongs to the box rather than to what is inside it, so it does not
    // move when the port is scrolled: only the contents do.
    let clip_scroll = if fragment.scroll_port.is_some() {
        page_scroll
    } else {
        scroll_y
    };
    // Every fragment inside a clipping box is clipped, without asking whether it
    // needs to be. It was asked once — whether the fragment fits inside the
    // rectangle — and that was wrong the moment the box scrolled: a fragment that
    // fits where the flow put it does not fit once it has been moved, and the
    // answer was computed before the move.
    let clip = fragment.clip;
    if let Some(clip) = clip {
        list.push(DisplayItem::PushLayer {
            blend: otlyra_gfx::peniko::BlendMode::default(),
            alpha: 1.0,
            transform: Affine::IDENTITY,
            clip: KurboRect::new(
                f64::from(clip.x),
                f64::from(clip.y - clip_scroll),
                f64::from(clip.right()),
                f64::from(clip.bottom() - clip_scroll),
            )
            .to_path(PATH_TOLERANCE),
        });
    }
    let origin = Affine::translate((f64::from(rect.x), f64::from(rect.y - scroll_y)));

    // Hit testing is a display list too, emitted into the same sequence as the
    // painting it belongs to. Keeping them together is what stops a link from being
    // clickable somewhere other than where it is drawn.
    if let Some(box_id) = fragment.box_id
        && !matches!(fragment.kind, FragmentKind::Line)
    {
        list.push(DisplayItem::HitTest {
            rect: KurboRect::new(
                f64::from(rect.x),
                f64::from(rect.y - scroll_y),
                f64::from(rect.right()),
                f64::from(rect.bottom() - scroll_y),
            ),
            transform: Affine::IDENTITY,
            id: HitTestId(otlyra_layout::box_id_to_u64(box_id)),
        });
    }

    match &fragment.kind {
        FragmentKind::Box => {
            // Shadows first: they are behind the box that casts them, and behind
            // each other in the order CSS paints them.
            for shadow in &fragment.style.shadows {
                if shadow.color.components[3] <= 0.0 {
                    continue;
                }
                let cast = Rect::new(
                    rect.x + shadow.x - shadow.spread,
                    rect.y + shadow.y - shadow.spread,
                    (rect.width + shadow.spread * 2.0).max(0.0),
                    (rect.height + shadow.spread * 2.0).max(0.0),
                );
                list.push(DisplayItem::Blurred {
                    transform: Affine::IDENTITY,
                    brush: Brush::Solid(shadow.color),
                    blur: f64::from(shadow.blur),
                    shape: shape_with_radii(cast, scroll_y, &fragment.style, shadow.spread),
                });
            }

            let background = fragment.style.background_color;
            // Transparent is the initial value, so most boxes paint nothing at all.
            if background.components[3] > 0.0 && !background_on_canvas {
                list.push(DisplayItem::Fill {
                    style: Fill::NonZero,
                    transform: Affine::IDENTITY,
                    brush: Brush::Solid(background),
                    brush_transform: None,
                    shape: box_shape(rect, scroll_y, &fragment.style),
                });
            }

            // A picture behind the box, over the colour and under the gradient —
            // which is the order CSS layers them in, topmost written first.
            if let Some(url) = fragment.style.background_image.as_deref()
                && !background_on_canvas
                && let Some(lookup) = background_picture
                && let Some(picture) = lookup(url)
                && picture.width > 0
                && picture.height > 0
                && rect.width > 0.0
                && rect.height > 0.0
            {
                let tiling = background_tiling(&fragment.style, rect, &picture);
                if tiling.covered.width > 0.0 && tiling.covered.height > 0.0 {
                    // A background belongs to its box: `cover` is meant to overflow
                    // and be cut off, not to spill onto whatever is drawn next. The
                    // box's own outline is the edge, so rounded corners cut the
                    // picture too.
                    list.push(DisplayItem::PushLayer {
                        blend: otlyra_gfx::peniko::BlendMode::default(),
                        alpha: 1.0,
                        transform: Affine::IDENTITY,
                        clip: box_shape(rect, scroll_y, &fragment.style),
                    });
                    list.push(DisplayItem::Fill {
                        style: Fill::NonZero,
                        transform: Affine::IDENTITY,
                        brush: Brush::Image(otlyra_gfx::peniko::ImageBrush {
                            image: picture,
                            sampler: tiling.sampler,
                        }),
                        brush_transform: Some(tiling.brush_transform(scroll_y)),
                        shape: KurboRect::new(
                            f64::from(tiling.covered.x),
                            f64::from(tiling.covered.y - scroll_y),
                            f64::from(tiling.covered.right()),
                            f64::from(tiling.covered.bottom() - scroll_y),
                        )
                        .to_path(PATH_TOLERANCE),
                    });
                    list.push(DisplayItem::PopLayer);
                }
            }

            // The gradient goes over the colour, which is the order CSS paints them
            // in: a box may name both, and the colour is what shows through where
            // the gradient is transparent.
            if let Some(gradient) = fragment.style.background_gradient.as_ref()
                && !background_on_canvas
            {
                list.push(DisplayItem::Fill {
                    style: Fill::NonZero,
                    transform: Affine::IDENTITY,
                    brush: Brush::Gradient(gradient_brush(gradient, rect, scroll_y)),
                    brush_transform: None,
                    shape: box_shape(rect, scroll_y, &fragment.style),
                });
            }

            paint_borders(list, fragment, rect, scroll_y);
        }

        FragmentKind::Line => {}

        FragmentKind::Image(image)
            if rect.width > 0.0 && rect.height > 0.0 && image.width > 0 && image.height > 0 =>
        {
            // The image carries its own pixel size, so the transform is what makes
            // it the size the page asked for: a scale to where `object-fit` put
            // it inside the fragment, then a move to where the fragment is.
            let placed = object_fit_rect(&fragment.style, rect.width, rect.height, image);
            let scale = Affine::scale_non_uniform(
                f64::from(placed.width) / f64::from(image.width),
                f64::from(placed.height) / f64::from(image.height),
            );
            let offset = Affine::translate((f64::from(placed.x), f64::from(placed.y)));
            // A picture larger than its box is cut off at the box, which is what
            // `cover` is for. The rectangle a clip takes is in the *image's* own
            // space, after the transform, so the box is expressed there — in
            // pixels of the file rather than pixels of the page.
            let clip_rect = (placed.x < 0.0
                || placed.y < 0.0
                || placed.width > rect.width
                || placed.height > rect.height)
                .then(|| {
                    let per_x = f64::from(image.width) / f64::from(placed.width.max(f32::EPSILON));
                    let per_y =
                        f64::from(image.height) / f64::from(placed.height.max(f32::EPSILON));
                    otlyra_gfx::kurbo::Rect::new(
                        f64::from(-placed.x) * per_x,
                        f64::from(-placed.y) * per_y,
                        f64::from(rect.width - placed.x) * per_x,
                        f64::from(rect.height - placed.y) * per_y,
                    )
                });
            list.push(DisplayItem::Image {
                image: otlyra_gfx::ImageResource::from(image.clone()),
                sampler: otlyra_gfx::peniko::ImageSampler::default(),
                transform: origin * offset * scale,
                clip_rect,
            });
        }

        FragmentKind::Image(_) => {}

        FragmentKind::Text(run) if !run.glyphs.is_empty() => {
            // Decorations first, so the glyphs sit on top of them: a line drawn
            // over text is a strikethrough whatever it was meant to be. The offset
            // and thickness come from the font, by way of the shaper.
            for decoration in [run.underline.as_ref(), run.strikethrough.as_ref()]
                .into_iter()
                .flatten()
            {
                let baseline = f64::from(run.glyphs[0].y);
                let top = f64::from(rect.y - scroll_y) + baseline - f64::from(decoration.offset);
                list.push(DisplayItem::Fill {
                    style: Fill::NonZero,
                    transform: Affine::IDENTITY,
                    brush: Brush::Solid(brush_to_color(run.brush)),
                    brush_transform: None,
                    shape: KurboRect::new(
                        f64::from(rect.x),
                        top,
                        f64::from(rect.x) + f64::from(run.advance),
                        top + f64::from(decoration.thickness),
                    )
                    .to_path(PATH_TOLERANCE),
                });
            }

            // The text's own shadows, behind it: the same glyphs, moved and
            // softened. A shadow has no spread — there is nothing to grow but the
            // letters themselves.
            for shadow in &fragment.style.text_shadows {
                if shadow.color.components[3] <= 0.0 {
                    continue;
                }
                list.push_glyph_run(
                    &run.font,
                    run.font_size,
                    run.normalized_coords.clone(),
                    Brush::Solid(shadow.color),
                    origin * Affine::translate((f64::from(shadow.x), f64::from(shadow.y))),
                    true,
                    f64::from(shadow.blur),
                    run.glyphs.clone(),
                );
            }

            list.push_glyphs(
                &run.font,
                run.font_size,
                run.normalized_coords.clone(),
                Brush::Solid(brush_to_color(run.brush)),
                origin,
                true,
                run.glyphs.clone(),
            );
        }

        FragmentKind::Text(_) => {}
    }

    if clip.is_some() {
        list.push(DisplayItem::PopLayer);
    }
}

/// How far a sticky box has been pushed from where the flow put it.
///
/// Zero until the page has scrolled far enough to take it past its inset, then
/// however much keeps it there — and never so far that it leaves its container,
/// which is what makes a sticky heading hand over to the next one.
fn sticky_shift(
    sticky: otlyra_layout::fragment::Sticky,
    scroll_y: f32,
    viewport_height: f32,
) -> f32 {
    let own = sticky.own;
    let container = sticky.container;

    if let Some(top) = sticky.top {
        let wanted = scroll_y + top - own.y;
        let room = (container.bottom() - own.bottom()).max(0.0);
        return wanted.clamp(0.0, room);
    }
    if let Some(bottom) = sticky.bottom {
        let wanted = (scroll_y + viewport_height - bottom) - own.bottom();
        let room = (container.y - own.y).min(0.0);
        return wanted.clamp(room, 0.0);
    }
    0.0
}

/// Where a background picture's tiles go.
///
/// One tile is placed, and the rest follow from it: a repeating axis is left to the
/// brush, which tiles a picture from wherever its transform puts it, and a
/// non-repeating one is handled by not painting past the one tile — which is what
/// `covered` is narrowed to. The alternative, an extend mode that puts nothing
/// outside the picture, is not one a brush has.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Tiling {
    /// The first tile: where the picture's own top left corner lands, and how
    /// large it is drawn.
    tile: Rect,
    /// The picture's own size in pixels, which the tile is scaled from.
    own: (f32, f32),
    /// The part of the box the picture reaches: the whole of it along an axis that
    /// repeats, one tile's worth along one that does not.
    covered: Rect,
    /// How the brush repeats around the tile.
    sampler: otlyra_gfx::peniko::ImageSampler,
}

impl Tiling {
    /// The brush's own transform: the tile's corner and the scale it is drawn at.
    fn brush_transform(&self, scroll_y: f32) -> Affine {
        Affine::translate((f64::from(self.tile.x), f64::from(self.tile.y - scroll_y)))
            * Affine::scale_non_uniform(
                f64::from(self.tile.width) / f64::from(self.own.0),
                f64::from(self.tile.height) / f64::from(self.own.1),
            )
    }
}

/// Work out that placement from the style, the box and the picture.
///
/// The area a picture is positioned in is the box inside its border, which is what
/// CSS positions a background against however far the painting itself spreads.
fn background_tiling(
    style: &otlyra_css::ComputedStyle,
    rect: Rect,
    picture: &otlyra_gfx::peniko::ImageData,
) -> Tiling {
    use otlyra_css::Repeat;
    use otlyra_gfx::peniko::Extend;

    let border = style.border;
    let area = Rect::new(
        rect.x + border.left.width,
        rect.y + border.top.width,
        (rect.width - border.left.width - border.right.width).max(0.0),
        (rect.height - border.top.width - border.bottom.width).max(0.0),
    );

    let own = (picture.width as f32, picture.height as f32);
    let (mut width, mut height) = background_extent(style, area, own);

    // `round` squeezes or stretches the tile so a whole number of them fits, which
    // is the whole of what it does — everything after it is the ordinary tiling.
    let rounded = |extent: f32, along: f32| {
        let count = (along / extent).round().max(1.0);
        along / count
    };
    if style.background_repeat.x == Repeat::Round && width > 0.0 {
        width = rounded(width, area.width);
    }
    if style.background_repeat.y == Repeat::Round && height > 0.0 {
        height = rounded(height, area.height);
    }

    let x = area.x + style.background_position.x.resolve(area.width - width);
    let y = area.y + style.background_position.y.resolve(area.height - height);
    let tile = Rect::new(x, y, width, height);

    // Along an axis that does not repeat, the picture reaches only as far as the
    // one tile; along one that does, as far as the box.
    let extent = |repeat: Repeat, start: f32, size: f32, area_start: f32, area_size: f32| {
        if repeat == Repeat::None {
            (start.max(area_start), size.min(area_size))
        } else {
            (area_start, area_size)
        }
    };
    let (left, covered_width) = extent(style.background_repeat.x, x, width, area.x, area.width);
    let (top, covered_height) = extent(style.background_repeat.y, y, height, area.y, area.height);

    let axis = |repeat: Repeat| match repeat {
        // Nothing is painted outside the one tile, so what the brush would put
        // there never shows; clamping is the cheapest answer that cannot smear.
        Repeat::None => Extend::Pad,
        Repeat::Repeat | Repeat::Round => Extend::Repeat,
    };

    Tiling {
        tile,
        own,
        covered: Rect::new(left, top, covered_width.max(0.0), covered_height.max(0.0)),
        sampler: otlyra_gfx::peniko::ImageSampler {
            x_extend: axis(style.background_repeat.x),
            y_extend: axis(style.background_repeat.y),
            ..Default::default()
        },
    }
}

/// How large one tile of a background picture is drawn.
///
/// `cover` and `contain` are the two that need the picture's own proportions: one
/// fills the area and is cropped, the other fits inside it whole.
fn background_extent(style: &otlyra_css::ComputedStyle, area: Rect, own: (f32, f32)) -> (f32, f32) {
    let (own_width, own_height) = own;
    let ratio = own_width / own_height.max(1.0);

    match style.background_size {
        otlyra_css::BackgroundSize::Auto => (own_width, own_height),
        otlyra_css::BackgroundSize::Fixed(width, height) => {
            (width.resolve(area.width), height.resolve(area.height))
        }
        otlyra_css::BackgroundSize::Cover => {
            if area.width / area.height.max(1.0) > ratio {
                (area.width, area.width / ratio)
            } else {
                (area.height * ratio, area.height)
            }
        }
        otlyra_css::BackgroundSize::Contain => {
            if area.width / area.height.max(1.0) > ratio {
                (area.height * ratio, area.height)
            } else {
                (area.width, area.width / ratio)
            }
        }
    }
}

/// The gradient a box's background is painted with, as a line across that box.
///
/// CSS gives the angle clockwise from pointing up, and the line is as long as the
/// box needs for the gradient to cover its corners — which is what makes a diagonal
/// gradient reach the ones it points at rather than stopping short of them.
fn gradient_brush(
    gradient: &otlyra_css::Gradient,
    rect: Rect,
    scroll_y: f32,
) -> otlyra_gfx::peniko::Gradient {
    use otlyra_gfx::kurbo::Point;

    let (width, height) = (f64::from(rect.width), f64::from(rect.height));
    let centre = Point::new(
        f64::from(rect.x) + width / 2.0,
        f64::from(rect.y - scroll_y) + height / 2.0,
    );

    // Up is negative y on the screen and zero degrees in CSS, and the angle turns
    // clockwise; the length is the specification's own, the projection of the box
    // onto the line.
    let angle = f64::from(gradient.angle);
    let (sin, cos) = angle.sin_cos();
    let length = (width * sin.abs() + height * cos.abs()) / 2.0;
    let along = Point::new(sin * length, -cos * length);

    let mut brush = otlyra_gfx::peniko::Gradient::new_linear(
        Point::new(centre.x - along.x, centre.y - along.y),
        Point::new(centre.x + along.x, centre.y + along.y),
    );
    for stop in &gradient.stops {
        brush.stops.push(otlyra_gfx::peniko::ColorStop {
            offset: stop.at,
            color: stop.color.into(),
        });
    }
    brush
}

/// The outline of a box: a rectangle, or a rounded one where `border-radius` says.
///
/// One radius per corner rather than an ellipse's two, and the radii are scaled
/// down together if they overlap — which is the rule CSS gives for a box asked for
/// rounder corners than it has room for.
/// Where inside its box a replaced element's picture is drawn.
///
/// The box is layout's answer and is not changed here: `object-fit` decides what
/// happens to the picture *inside* it, and a picture that comes out larger is cut
/// off by the box rather than making it bigger. The offsets come from
/// `object-position`, which is the same arithmetic as a background's and starts
/// in the middle rather than the corner.
fn object_fit_rect(
    style: &otlyra_css::ComputedStyle,
    box_width: f32,
    box_height: f32,
    image: &otlyra_gfx::peniko::ImageData,
) -> Placed {
    use otlyra_css::ObjectFit;

    let own = (image.width as f32, image.height as f32);
    let contain = (box_width / own.0).min(box_height / own.1);
    let (width, height) = match style.object_fit {
        ObjectFit::Fill => (box_width, box_height),
        ObjectFit::Contain => (own.0 * contain, own.1 * contain),
        ObjectFit::Cover => {
            let cover = (box_width / own.0).max(box_height / own.1);
            (own.0 * cover, own.1 * cover)
        }
        ObjectFit::None => own,
        ObjectFit::ScaleDown => {
            let scale = contain.min(1.0);
            (own.0 * scale, own.1 * scale)
        }
    };

    let position = style.object_position;
    Placed {
        x: position.x.resolve(box_width - width),
        y: position.y.resolve(box_height - height),
        width,
        height,
    }
}

/// A picture's place inside its box, relative to the box's own corner.
struct Placed {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn box_shape(rect: Rect, scroll_y: f32, style: &otlyra_css::ComputedStyle) -> BezPath {
    shape_with_radii(rect, scroll_y, style, 0.0)
}

/// The same outline, grown by `spread` at every corner as well as every edge.
///
/// A shadow spread outwards is not the box's own curve moved: the specification
/// grows each non-zero radius by the spread, so the shadow of a rounded box stays
/// the same shape rather than turning into a rounded rectangle with tighter corners.
fn shape_with_radii(
    rect: Rect,
    scroll_y: f32,
    style: &otlyra_css::ComputedStyle,
    spread: f32,
) -> BezPath {
    let bounds = KurboRect::new(
        f64::from(rect.x),
        f64::from(rect.y - scroll_y),
        f64::from(rect.right()),
        f64::from(rect.bottom() - scroll_y),
    );

    if !style.radius.any() {
        return bounds.to_path(PATH_TOLERANCE);
    }

    let corner = |value: otlyra_css::Length| {
        let radius = value.resolve(rect.width);
        if radius <= 0.0 {
            // A square corner stays square however far the shadow spreads.
            return 0.0;
        }
        f64::from((radius + spread).max(0.0))
    };
    let mut radii = [
        corner(style.radius.top_left),
        corner(style.radius.top_right),
        corner(style.radius.bottom_right),
        corner(style.radius.bottom_left),
    ];

    // Two radii along one edge cannot together be longer than the edge.
    let width = f64::from(rect.width);
    let height = f64::from(rect.height);
    let scale = [
        (radii[0] + radii[1], width),
        (radii[2] + radii[3], width),
        (radii[0] + radii[3], height),
        (radii[1] + radii[2], height),
    ]
    .iter()
    .filter(|(sum, _)| *sum > 0.0)
    .map(|(sum, edge)| edge / sum)
    .fold(1.0_f64, f64::min);
    if scale < 1.0 {
        for radius in &mut radii {
            *radius *= scale;
        }
    }

    otlyra_gfx::kurbo::RoundedRect::from_rect(
        bounds,
        otlyra_gfx::kurbo::RoundedRectRadii::new(radii[0], radii[1], radii[2], radii[3]),
    )
    .to_path(PATH_TOLERANCE)
}

/// The colour a shaped run carried, back as a paint colour.
/// The four borders of a box, each as a filled rectangle on the inside edge of the
/// border box.
///
/// Rectangles rather than a stroked outline, because each side has its own width
/// and colour and a stroke has one of each. The corners are square: mitring them
/// needs the four trapezia CSS specifies, and the difference only shows where two
/// adjacent sides differ in colour and are thick enough to see.
fn paint_borders(
    list: &mut DisplayList,
    fragment: &Fragment,
    rect: otlyra_layout::Rect,
    scroll_y: f32,
) {
    let border = fragment.style.border;
    let (left, top) = (f64::from(rect.x), f64::from(rect.y - scroll_y));
    let (right, bottom) = (f64::from(rect.right()), f64::from(rect.bottom() - scroll_y));

    // A rounded box's border follows its corners, which four rectangles cannot do.
    // One stroke can, as long as every side is the same — and a border that is
    // rounded and different on each side is a shape CSS defines and nobody writes.
    let uniform =
        border.top == border.right && border.right == border.bottom && border.bottom == border.left;
    if fragment.style.radius.any() && uniform {
        let side = border.top;
        if !side.is_visible() {
            return;
        }
        let width = f64::from(side.width);
        // Strokes straddle the path, so the path is inset by half the width to put
        // the whole of it inside the box — where CSS draws it.
        let inset = otlyra_layout::Rect::new(
            rect.x + side.width / 2.0,
            rect.y + side.width / 2.0,
            (rect.width - side.width).max(0.0),
            (rect.height - side.width).max(0.0),
        );
        list.push(DisplayItem::Stroke {
            style: otlyra_gfx::kurbo::Stroke::new(width),
            transform: Affine::IDENTITY,
            brush: Brush::Solid(side.color),
            brush_transform: None,
            shape: box_shape(inset, scroll_y, &fragment.style),
        });
        return;
    }

    let sides = [
        (
            border.top,
            [left, top, right, top + f64::from(border.top.width)],
        ),
        (
            border.right,
            [right - f64::from(border.right.width), top, right, bottom],
        ),
        (
            border.bottom,
            [left, bottom - f64::from(border.bottom.width), right, bottom],
        ),
        (
            border.left,
            [left, top, left + f64::from(border.left.width), bottom],
        ),
    ];

    for (side, [x0, y0, x1, y1]) in sides {
        if !side.is_visible() {
            continue;
        }
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Solid(side.color),
            brush_transform: None,
            shape: KurboRect::new(x0, y0, x1, y1).to_path(PATH_TOLERANCE),
        });
    }
}

fn brush_to_color(brush: [u8; 4]) -> Color {
    Color::from_rgba8(brush[0], brush[1], brush[2], brush[3])
}

#[cfg(test)]
mod tests {
    use otlyra_gfx::{PaintOp, RecordingPainter, render};
    use otlyra_layout::{Viewport, build_box_tree, layout};
    use otlyra_text::TextEngine;

    use super::*;

    fn page(html: &str, scroll_y: f32) -> DisplayList {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let boxes = build_box_tree(&parsed.document);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
            &mut text,
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );
        build_display_list(&fragments, (800.0, 600.0), scroll_y)
    }

    /// A page with one picture in it, at `width` by `height` logical pixels.
    fn page_with_image(style: &str, pixels: (u32, u32)) -> DisplayList {
        let html = format!("<style>body {{ margin: 0 }} {style}</style><img src=a.png>");
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let image = otlyra_gfx::peniko::ImageData {
            data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![
                0u8;
                pixels.0 as usize
                    * pixels.1 as usize
                    * 4
            ])),
            format: otlyra_gfx::peniko::ImageFormat::Rgba8,
            alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
            width: pixels.0,
            height: pixels.1,
        };
        let styles = otlyra_css::cascade::style_document(
            &parsed.document,
            otlyra_css::cascade::Viewport {
                width: 800.0,
                height: 600.0,
                scale: 1.0,
                text_scale: 1.0,
                color_scheme: Default::default(),
            },
        );
        let images: otlyra_layout::Images = otlyra_layout::image_sources(
            &parsed.document,
            otlyra_css::cascade::Viewport::default(),
        )
        .into_iter()
        .map(|source| (source.node, otlyra_layout::Picture::new(image.clone())))
        .collect();
        let boxes =
            otlyra_layout::build_box_tree_with_images(&parsed.document, Some(&styles), &images);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
            &mut text,
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );
        build_display_list(&fragments, (800.0, 600.0), 0.0)
    }

    /// A page laid out with its own stylesheet, scrolled to `scroll_y`.
    fn styled_page(html: &str, scroll_y: f32) -> DisplayList {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let styles = otlyra_css::cascade::style_document(
            &parsed.document,
            otlyra_css::cascade::Viewport {
                width: 800.0,
                height: 600.0,
                scale: 1.0,
                text_scale: 1.0,
                color_scheme: Default::default(),
            },
        );
        let boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
            &mut text,
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );
        build_display_list(&fragments, (800.0, 600.0), scroll_y)
    }

    /// The order the fills come out in, by colour, which is the painting order.
    fn fill_order(list: &DisplayList) -> Vec<Color> {
        list.items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    ..
                } => Some(*colour),
                _ => None,
            })
            .collect()
    }

    /// A positioned box paints over the boxes it overlaps, whatever document order
    /// says; `z-index` moves it further up, or below the flow entirely.
    #[test]
    fn a_positioned_box_paints_above_the_flow_and_z_index_moves_it() {
        let red = Color::from_rgb8(255, 0, 0);
        let blue = Color::from_rgb8(0, 0, 255);

        // The positioned box is written first, so document order alone would paint
        // it under the one after it.
        let over = fill_order(&styled_page(
            "<style>body { margin: 0 } div { height: 50px }              .a { position: relative; background: rgb(255, 0, 0) }              .b { background: rgb(0, 0, 255) }</style>             <div class=a></div><div class=b></div>",
            0.0,
        ));
        let (red_at, blue_at) = (
            over.iter().position(|colour| *colour == red),
            over.iter().position(|colour| *colour == blue),
        );
        assert!(
            red_at > blue_at,
            "the positioned box painted under the flow"
        );

        let under = fill_order(&styled_page(
            "<style>body { margin: 0 } div { height: 50px }              .a { position: relative; z-index: -1; background: rgb(255, 0, 0) }              .b { background: rgb(0, 0, 255) }</style>             <div class=a></div><div class=b></div>",
            0.0,
        ));
        assert!(
            under.iter().position(|colour| *colour == red)
                < under.iter().position(|colour| *colour == blue),
            "a negative z-index must paint below the flow"
        );
    }

    /// `z-index` orders a box against its siblings, not against the page.
    ///
    /// A box with a large index inside a box with a small one stays under
    /// everything the small one is under: the large number is compared only with
    /// the numbers written inside the same positioned ancestor.
    #[test]
    fn a_large_index_inside_a_small_one_stays_inside_it() {
        let order = fill_order(&styled_page(
            "<style>body { margin: 0 } .box { position: absolute; width: 100px; height: 60px } \
             .outer { left: 0; top: 0; z-index: 1; background: rgb(255, 0, 0) } \
             .inner { left: 20px; top: 20px; z-index: 100; background: rgb(0, 0, 255) } \
             .beside { left: 40px; top: 10px; z-index: 2; background: rgb(0, 255, 0) }</style> \
             <div class='box outer'><div class='box inner'></div></div> \
             <div class='box beside'></div>",
            0.0,
        ));

        let at = |colour: Color| order.iter().position(|painted| *painted == colour);
        let (outer, inner, beside) = (
            at(Color::from_rgb8(255, 0, 0)),
            at(Color::from_rgb8(0, 0, 255)),
            at(Color::from_rgb8(0, 255, 0)),
        );
        assert!(
            outer < inner,
            "a box paints under what is inside it: {order:?}"
        );
        assert!(
            inner < beside,
            "an index of 100 inside a 1 is still under a 2: {order:?}"
        );
    }

    /// A selection is drawn behind the text it covers: the highlight is pushed
    /// before the glyphs of the run it belongs to, so the letters are drawn over it.
    #[test]
    fn a_selection_is_drawn_under_the_text() {
        let parsed = otlyra_html::parse(b"<body><p>one two three</p>", Some("utf-8"));
        let styles = otlyra_css::cascade::style_document(
            &parsed.document,
            otlyra_css::cascade::Viewport {
                width: 800.0,
                height: 600.0,
                scale: 1.0,
                text_scale: 1.0,
                color_scheme: Default::default(),
            },
        );
        let boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
            &mut text,
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );

        let start = otlyra_layout::selection::position_at(&fragments, 0.0, 20.0).expect("a place");
        let end = otlyra_layout::selection::position_at(&fragments, 60.0, 20.0).expect("another");
        let highlight = otlyra_layout::selection::rects(
            &fragments,
            otlyra_layout::Selection {
                anchor: start,
                focus: end,
            },
        );
        assert!(!highlight.is_empty(), "something is selected");

        let list = build_display_list_with(
            &fragments,
            &Frame {
                viewport: (800.0, 600.0),
                selection: &highlight,
                ..Frame::default()
            },
        );

        let mut painted = None;
        for item in list.items() {
            match item {
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    shape,
                    ..
                } if *colour == SELECTION => painted = Some(shape.bounding_box()),
                DisplayItem::Glyphs { .. } => {
                    assert!(
                        painted.is_some(),
                        "the glyphs were drawn before the highlight under them"
                    );
                    break;
                }
                _ => {}
            }
        }
        let painted = painted.expect("the highlight was drawn");
        assert!(
            painted.width() > 0.0 && painted.width() < 200.0,
            "it covers the words it was asked for and no more: {painted:?}"
        );
    }

    /// A transformed box draws where its transform puts it, and so does everything
    /// inside it — including the region a click is tested against, which is the
    /// same item with the same transform on it.
    #[test]
    fn a_transform_moves_the_drawing_and_what_is_tested_against_it() {
        let list = styled_page(
            "<style>body { margin: 0 } .card { width: 100px; height: 50px; \
             background: rgb(255, 0, 0) } .moved { transform: translate(200px, 100px) }</style> \
             <div class='card moved'><a href='/somewhere'>a link inside it</a></div>",
            0.0,
        );

        let red = list
            .items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    transform,
                    shape,
                    ..
                } if *colour == Color::from_rgb8(255, 0, 0) => {
                    Some(transform.transform_rect_bbox(shape.bounding_box()))
                }
                _ => None,
            })
            .expect("the card");
        assert!(
            (red.x0 - 200.0).abs() < 0.01 && (red.y0 - 100.0).abs() < 0.01,
            "the box is drawn where the transform puts it: {red:?}"
        );

        // The link inside it is hit where it is drawn, not where it was laid out:
        // the two points land on different things, and the near one on the link.
        let drawn = otlyra_gfx::hit_test(&list, (210.0, 110.0)).expect("something is drawn there");
        let empty = otlyra_gfx::hit_test(&list, (10.0, 10.0));
        assert!(
            empty.is_none_or(|hit| hit.id != drawn.id),
            "the link is still where it was laid out rather than where it is drawn"
        );
    }

    /// The steps of a transform apply in the order they were written, and about
    /// the box's own middle unless it says otherwise.
    #[test]
    fn transform_steps_apply_in_order_and_about_the_origin() {
        let corner = |css: &str| {
            let list = styled_page(
                &format!(
                    "<style>body {{ margin: 0 }} .card {{ width: 100px; height: 100px; \
                     background: rgb(255, 0, 0); {css} }}</style><div class=card></div>"
                ),
                0.0,
            );
            list.items()
                .iter()
                .find_map(|item| match item {
                    DisplayItem::Fill {
                        brush: Brush::Solid(colour),
                        transform,
                        shape,
                        ..
                    } if *colour == Color::from_rgb8(255, 0, 0) => {
                        Some(transform.transform_rect_bbox(shape.bounding_box()))
                    }
                    _ => None,
                })
                .expect("the card")
        };

        // Turned about its middle, a square keeps its middle and grows its box.
        let turned = corner("transform: rotate(45deg)");
        assert!(
            (turned.center().x - 50.0).abs() < 0.01 && (turned.center().y - 50.0).abs() < 0.01,
            "a box turns about its own middle: {turned:?}"
        );
        assert!(turned.width() > 140.0, "and its bounds grow: {turned:?}");

        // About the corner instead, the middle moves.
        let cornered = corner("transform: rotate(45deg); transform-origin: 0 0");
        assert!(
            (cornered.y0 - 0.0).abs() < 0.01 && cornered.center().y > 60.0,
            "an origin of its own turns it about that corner: {cornered:?}"
        );

        // Order matters: moving then turning is not turning then moving.
        let first = corner("transform: translate(50px, 0) rotate(30deg)");
        let second = corner("transform: rotate(30deg) translate(50px, 0)");
        assert!(
            (first.center().x - second.center().x).abs() > 1.0
                || (first.center().y - second.center().y).abs() > 1.0,
            "the steps are applied in the order written: {first:?} against {second:?}"
        );
    }

    /// A half-transparent box and everything in it is composited once: one layer
    /// opened before the box and closed after the last thing inside it.
    #[test]
    fn opacity_composites_a_box_and_its_contents_as_one_group() {
        let list = styled_page(
            "<style>body { margin: 0 } .plate { width: 100px; height: 50px; \
             background: rgb(255, 0, 0) } .half { opacity: 0.5 } \
             .over { position: absolute; left: 10px; top: 10px; width: 20px; height: 20px; \
             background: rgb(0, 0, 255) }</style> \
             <div class='plate half'><div class=over></div></div><div class=plate></div>",
            0.0,
        );

        let mut depth = 0i32;
        let mut inside = Vec::new();
        let mut outside = Vec::new();
        let mut alpha = None;
        for item in list.items() {
            match item {
                DisplayItem::PushLayer { alpha: value, .. } => {
                    depth += 1;
                    alpha = Some(*value);
                }
                DisplayItem::PopLayer => depth -= 1,
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    ..
                } => {
                    if depth > 0 {
                        inside.push(*colour);
                    } else {
                        outside.push(*colour);
                    }
                }
                _ => {}
            }
        }

        assert_eq!(depth, 0, "every layer opened was closed");
        assert_eq!(alpha, Some(0.5), "the group carries the box's opacity");
        assert!(
            inside.contains(&Color::from_rgb8(255, 0, 0))
                && inside.contains(&Color::from_rgb8(0, 0, 255)),
            "the box and the positioned box inside it are both in the group: {inside:?}"
        );
        assert!(
            outside.contains(&Color::from_rgb8(255, 0, 0)),
            "the opaque box after it is not: {outside:?}"
        );
    }

    /// A negative index inside a positioned box paints over that box's background
    /// and under its content, rather than dropping below the page's flow.
    #[test]
    fn a_negative_index_inside_a_positioned_box_stays_inside_it() {
        let order = fill_order(&styled_page(
            "<style>body { margin: 0 } .flow { height: 80px; background: rgb(0, 255, 0) } \
             .parent { position: absolute; left: 0; top: 0; width: 200px; height: 120px; \
             background: rgb(255, 0, 0) } \
             .under { position: absolute; left: 20px; top: 20px; width: 100px; height: 60px; \
             background: rgb(0, 0, 255); z-index: -1 }</style> \
             <div class=flow></div><div class=parent><div class=under></div></div>",
            0.0,
        ));

        let at = |colour: Color| order.iter().position(|painted| *painted == colour);
        let (flow, parent, under) = (
            at(Color::from_rgb8(0, 255, 0)),
            at(Color::from_rgb8(255, 0, 0)),
            at(Color::from_rgb8(0, 0, 255)),
        );
        assert!(
            flow < parent && parent < under,
            "a negative index inside a positioned box paints over that box, \
             not under the page: {order:?}"
        );
    }

    /// The same for an absolutely positioned box, which takes a different path
    /// through layout than a relative one.
    #[test]
    fn an_absolute_box_with_a_negative_z_index_paints_under_the_flow() {
        let order = fill_order(&styled_page(
            "<style>body { margin: 0 } .row { position: relative; height: 90px }              .flow { height: 90px; background: rgb(0, 0, 255) }              .under { position: absolute; left: 20px; top: 20px; width: 100px;              height: 40px; background: rgb(255, 0, 0); z-index: -1 }</style>             <div class=row><div class=flow>flow</div><div class=under>under</div></div>",
            0.0,
        ));
        let red = order.iter().position(|c| *c == Color::from_rgb8(255, 0, 0));
        let blue = order.iter().position(|c| *c == Color::from_rgb8(0, 0, 255));
        assert!(red.is_some() && blue.is_some(), "both boxes painted");
        assert!(red < blue, "the negative z-index painted over the flow");
    }

    /// The root element's background belongs to the canvas, not to the element:
    /// it covers the viewport however short the document is, and it is not painted
    /// a second time over whatever a negative `z-index` put below the flow.
    #[test]
    fn the_root_background_is_the_canvas_and_is_painted_once() {
        let list = styled_page(
            "<style>html { background: rgb(0, 128, 0) } \
             .below { position: relative; z-index: -1; background: rgb(255, 0, 0); \
             height: 40px }</style><div class=below>below</div>",
            0.0,
        );
        let greens: Vec<_> = list
            .items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    shape,
                    ..
                } if *colour == Color::from_rgb8(0, 128, 0) => Some(shape.bounding_box()),
                _ => None,
            })
            .collect();

        assert_eq!(greens.len(), 1, "the root background was painted twice");
        assert_eq!(
            greens[0].y1, 600.0,
            "and not only as far as the content goes"
        );
        assert!(
            fill_order(&list)
                .iter()
                .position(|colour| *colour == Color::from_rgb8(255, 0, 0))
                > Some(0),
            "the box below the flow still paints over the canvas"
        );
    }

    /// `overflow: hidden` cuts its contents off, which reaches the rasterizer as a
    /// clip layer around whatever is inside the box — and every layer pushed is
    /// popped, or everything after it would be clipped too.
    #[test]
    fn a_clipping_box_wraps_its_contents_in_a_layer() {
        let list = styled_page(
            "<style>body { margin: 0 } \
             .card { overflow: hidden; height: 40px; width: 100px } \
             .tall { height: 200px; background: rgb(255, 0, 0) }</style>\
             <div class=card><div class=tall>tall</div></div>",
            0.0,
        );

        let mut depth = 0i32;
        let mut deepest = 0i32;
        let mut clips = Vec::new();
        for item in list.items() {
            match item {
                DisplayItem::PushLayer { clip, .. } => {
                    depth += 1;
                    deepest = deepest.max(depth);
                    clips.push(clip.bounding_box());
                }
                DisplayItem::PopLayer => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "a layer was popped that was never pushed");
        }

        assert_eq!(depth, 0, "a layer was left open");
        assert!(deepest > 0, "nothing was clipped");
        assert_eq!(clips[0].y1, 40.0, "cut off at the box, not at its contents");
    }

    /// A scrollbar says two things and nothing else: how much of the content is on
    /// screen, and how far through it the reader is. Content that fits gets none.
    #[test]
    fn a_scrollbar_shows_where_the_reader_is_and_only_when_there_is_somewhere_to_go() {
        let thumb = |list: &DisplayList| -> Option<otlyra_gfx::kurbo::Rect> {
            list.items()
                .iter()
                .filter_map(|item| match item {
                    DisplayItem::Fill {
                        brush: Brush::Solid(colour),
                        shape,
                        ..
                    } if *colour == SCROLLBAR_THUMB => Some(shape.bounding_box()),
                    _ => None,
                })
                .next_back()
        };

        let short = "<style>body { margin: 0 } p { height: 100px }</style><p>short</p>";
        assert!(
            thumb(&styled_page(short, 0.0)).is_none(),
            "a page that fits was given a scrollbar"
        );

        let long = "<style>body { margin: 0 } p { height: 3000px }</style><p>long</p>";
        let at_top = thumb(&styled_page(long, 0.0)).expect("a scrollbar");
        let further = thumb(&styled_page(long, 1000.0)).expect("a scrollbar");

        assert!(
            at_top.y0.abs() < 0.01,
            "at the top of the page it is at the top"
        );
        assert!(further.y0 > at_top.y0, "it did not move with the reader");
        assert!(
            at_top.height() < 600.0 / 4.0,
            "a fifth of the content on screen should be a short thumb"
        );
        assert!(at_top.x1 <= 800.0, "it is drawn inside the viewport");
    }

    /// `border-radius` rounds the background and the border together, and a radius
    /// larger than the box is scaled down rather than folding over itself.
    #[test]
    fn a_rounded_box_is_drawn_round() {
        use otlyra_gfx::kurbo::PathEl;

        let curves = |html: &str| -> usize {
            styled_page(html, 0.0)
                .items()
                .iter()
                .filter_map(|item| match item {
                    DisplayItem::Fill { shape, .. } | DisplayItem::Stroke { shape, .. } => {
                        Some(shape)
                    }
                    _ => None,
                })
                .flat_map(|shape| shape.elements())
                .filter(|element| matches!(element, PathEl::CurveTo(..) | PathEl::QuadTo(..)))
                .count()
        };

        let square = "<style>body { margin: 0 } div { background: rgb(0, 0, 255); height: 40px }                      </style><div></div>";
        let round = "<style>body { margin: 0 } div { background: rgb(0, 0, 255); height: 40px;                      border-radius: 8px }</style><div></div>";
        assert_eq!(curves(square), 0, "a square box has no curves in it");
        assert!(curves(round) > 0, "a rounded one does");

        // A pill: the radius is larger than the box and has to be scaled down, or
        // the corners would overlap and the path would fold over itself.
        let pill = "<style>body { margin: 0 } div { background: rgb(0, 0, 255); height: 40px;                     width: 100px; border-radius: 999px }</style><div></div>";
        let bounds = styled_page(pill, 0.0)
            .items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    shape,
                    ..
                } if *colour == Color::from_rgb8(0, 0, 255) => Some(shape.bounding_box()),
                _ => None,
            })
            .expect("the box");
        assert_eq!(bounds.width(), 100.0, "it is still the size it was");
        assert_eq!(bounds.height(), 40.0);
    }

    /// A gradient background reaches the rasterizer as a gradient, with its stops
    /// in order and its line pointing where CSS says.
    #[test]
    fn a_gradient_background_is_painted_as_one() {
        let gradient = |css: &str| {
            let html = format!(
                "<style>body {{ margin: 0 }} div {{ height: 100px; width: 200px; \
                 background: {css} }}</style><div></div>"
            );
            styled_page(&html, 0.0)
                .items()
                .iter()
                .find_map(|item| match item {
                    DisplayItem::Fill {
                        brush: Brush::Gradient(gradient),
                        ..
                    } => Some(gradient.clone()),
                    _ => None,
                })
                .expect("a gradient")
        };

        let down = gradient("linear-gradient(rgb(255, 0, 0), rgb(0, 0, 255))");
        assert_eq!(down.stops.len(), 2);
        assert_eq!(down.stops[0].offset, 0.0);
        assert_eq!(down.stops[1].offset, 1.0);
        let otlyra_gfx::peniko::GradientKind::Linear(line) = down.kind else {
            panic!("a linear gradient");
        };
        assert!(line.end.y > line.start.y, "the default runs down the box");
        assert!(
            (line.start.x - line.end.x).abs() < 0.01,
            "and straight down, not across"
        );

        let across = gradient("linear-gradient(to right, rgb(255, 0, 0), rgb(0, 0, 255))");
        let otlyra_gfx::peniko::GradientKind::Linear(line) = across.kind else {
            panic!("a linear gradient");
        };
        assert!(line.end.x > line.start.x, "to right runs across the box");
        assert!((line.start.y - line.end.y).abs() < 0.01);

        // Stops without a position are spread evenly.
        let three = gradient("linear-gradient(rgb(255,0,0), rgb(0,255,0), rgb(0,0,255))");
        assert_eq!(three.stops.len(), 3);
        assert!((three.stops[1].offset - 0.5).abs() < 0.001);
    }

    /// A shadow is drawn behind the box that casts it, offset and blurred as the
    /// page asked, and a spread grows its corners with it.
    #[test]
    fn a_box_shadow_is_cast_behind_the_box() {
        let list = styled_page(
            "<style>body { margin: 0 } \
             div { height: 40px; width: 100px; background: rgb(0, 128, 0); \
             border-radius: 6px; box-shadow: 4px 8px 12px 2px rgb(0, 0, 0) }</style>\
             <div></div>",
            0.0,
        );

        let (blur, bounds) = list
            .items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Blurred { shape, blur, .. } => Some((*blur, shape.bounding_box())),
                _ => None,
            })
            .expect("a shadow");

        assert_eq!(blur, 12.0, "the CSS radius reaches the rasterizer as it is");
        // Offset by four and eight, grown by two on every side.
        assert_eq!(bounds.x0, 2.0);
        assert_eq!(bounds.y0, 6.0);
        assert_eq!(bounds.width(), 104.0);
        assert_eq!(bounds.height(), 44.0);

        // Behind the box: the shadow comes first in the list.
        let shadow_index = list
            .items()
            .iter()
            .position(|item| matches!(item, DisplayItem::Blurred { .. }))
            .expect("a shadow");
        let background = list
            .items()
            .iter()
            .position(|item| {
                matches!(item, DisplayItem::Fill { brush: Brush::Solid(colour), .. }
                    if *colour == Color::from_rgb8(0, 128, 0))
            })
            .expect("the background");
        assert!(shadow_index < background, "the shadow painted over its box");
    }

    /// A text shadow is the same run drawn behind itself, moved and softened.
    #[test]
    fn text_shadows_are_drawn_behind_the_text() {
        let list = styled_page(
            "<style>body { margin: 0 } \
             p { color: rgb(0, 0, 0); text-shadow: 2px 3px 4px rgb(255, 0, 0) }</style>\
             <p>text</p>",
            0.0,
        );

        let runs: Vec<_> = list
            .items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Glyphs {
                    brush: Brush::Solid(colour),
                    transform,
                    blur,
                    ..
                } => Some((*colour, transform.as_coeffs(), *blur)),
                _ => None,
            })
            .collect();

        assert_eq!(runs.len(), 2, "one shadow and the text itself");
        let (shadow, shadow_at, blur) = runs[0];
        let (text, text_at, text_blur) = runs[1];

        assert_eq!(
            shadow,
            Color::from_rgb8(255, 0, 0),
            "the shadow comes first"
        );
        assert_eq!(text, Color::from_rgb8(0, 0, 0));
        assert_eq!(blur, 4.0);
        assert_eq!(text_blur, 0.0, "the text itself is not blurred");
        assert_eq!(shadow_at[4] - text_at[4], 2.0, "moved right by two");
        assert_eq!(shadow_at[5] - text_at[5], 3.0, "and down by three");
    }

    /// The one tile a background picture is placed by: where it starts, how large
    /// it is, and how far the fill that repeats it reaches.
    fn background_tile(declarations: &str) -> (Rect, Rect, otlyra_gfx::peniko::ImageSampler) {
        let source = format!(
            "<style>body {{ margin: 0 }} div {{ height: 50px; width: 200px; \
             background-image: url(behind.png); {declarations} }}</style><div></div>"
        );
        let parsed = otlyra_html::parse(source.as_bytes(), Some("utf-8"));
        let styles = otlyra_css::cascade::style_document(
            &parsed.document,
            otlyra_css::cascade::Viewport {
                width: 800.0,
                height: 600.0,
                scale: 1.0,
                text_scale: 1.0,
                color_scheme: Default::default(),
            },
        );
        let boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
            &mut text,
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );

        // A twenty by ten picture, so its own proportions are visible in what
        // `cover` and `contain` make of it.
        let picture = otlyra_gfx::peniko::ImageData {
            data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![0u8; 20 * 10 * 4])),
            format: otlyra_gfx::peniko::ImageFormat::Rgba8,
            alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
            width: 20,
            height: 10,
        };

        let list = build_display_list_with(
            &fragments,
            &Frame {
                viewport: (800.0, 600.0),
                background: Some(&|url: &str| (url == "behind.png").then(|| picture.clone())),
                ..Frame::default()
            },
        );

        list.items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Image(image),
                    brush_transform: Some(transform),
                    shape,
                    ..
                } => {
                    let coeffs = transform.as_coeffs();
                    let tile = Rect::new(
                        coeffs[4] as f32,
                        coeffs[5] as f32,
                        (coeffs[0] * f64::from(image.image.width)) as f32,
                        (coeffs[3] * f64::from(image.image.height)) as f32,
                    );
                    let covered = shape.bounding_box();
                    Some((
                        tile,
                        Rect::new(
                            covered.x0 as f32,
                            covered.y0 as f32,
                            covered.width() as f32,
                            covered.height() as f32,
                        ),
                        image.sampler,
                    ))
                }
                _ => None,
            })
            .expect("a background picture")
    }

    /// A picture named by a rule is drawn behind the box, sized as the rule says.
    #[test]
    fn a_background_picture_is_drawn_behind_its_box() {
        let (tile, _, _) = background_tile("background-size: cover");
        assert_eq!(tile.width, 200.0, "cover fills the box across");
        assert_eq!(
            tile.height, 100.0,
            "and overflows it down rather than squashing"
        );

        let (tile, _, _) = background_tile("background-size: contain");
        assert_eq!(tile.width, 100.0, "contain fits inside the box");
        assert_eq!(tile.height, 50.0, "whole");
    }

    /// A picture tiles by default, and the fill that carries it covers the box; one
    /// told not to repeat covers exactly one tile, which is what keeps a brush that
    /// has no way to put nothing outside a picture from smearing its edge.
    #[test]
    fn a_background_picture_tiles_unless_told_not_to() {
        use otlyra_gfx::peniko::Extend;

        let (tile, covered, sampler) = background_tile("");
        assert_eq!((tile.width, tile.height), (20.0, 10.0), "its own size");
        assert_eq!(covered, Rect::new(0.0, 0.0, 200.0, 50.0), "the whole box");
        assert_eq!(sampler.x_extend, Extend::Repeat);
        assert_eq!(sampler.y_extend, Extend::Repeat);

        let (_, covered, sampler) = background_tile("background-repeat: no-repeat");
        assert_eq!(covered, Rect::new(0.0, 0.0, 20.0, 10.0), "one tile");
        assert_eq!(sampler.x_extend, Extend::Pad);

        let (_, covered, sampler) = background_tile("background-repeat: repeat-x");
        assert_eq!(
            covered,
            Rect::new(0.0, 0.0, 200.0, 10.0),
            "a band across the box"
        );
        assert_eq!(sampler.x_extend, Extend::Repeat);
        assert_eq!(sampler.y_extend, Extend::Pad);
    }

    /// A position moves the tile within the room the picture leaves in its box,
    /// which is why a percentage is not a percentage of the box.
    #[test]
    fn a_background_position_moves_the_tile_by_what_is_left_over() {
        let (tile, _, _) =
            background_tile("background-repeat: no-repeat; background-position: 0 0");
        assert_eq!((tile.x, tile.y), (0.0, 0.0));

        // 180 across and 40 down are what a twenty by ten picture leaves.
        let (tile, _, _) =
            background_tile("background-repeat: no-repeat; background-position: 50% 50%");
        assert_eq!((tile.x, tile.y), (90.0, 20.0));

        let (tile, _, _) =
            background_tile("background-repeat: no-repeat; background-position: right bottom");
        assert_eq!((tile.x, tile.y), (180.0, 40.0));

        let (tile, _, _) = background_tile(
            "background-repeat: no-repeat; background-position: right 10px top 4px",
        );
        assert_eq!((tile.x, tile.y), (170.0, 4.0));
    }

    /// `round` squeezes the tile so a whole number of them fits the box, which is
    /// the whole of the difference between it and `repeat`.
    #[test]
    fn round_fits_a_whole_number_of_tiles() {
        // A thirty-pixel tile across two hundred: seven of them at 28.57 rather
        // than six and two thirds at thirty.
        let (tile, _, _) = background_tile("background-repeat: round; background-size: 30px 10px");
        assert!(
            (tile.width - 200.0 / 7.0).abs() < 0.01,
            "tile was {} wide",
            tile.width
        );
        assert!((tile.height - 10.0).abs() < 0.01, "and untouched down");
    }

    /// A background is positioned against the box inside its border, however far
    /// the painting itself spreads.
    #[test]
    fn a_background_is_positioned_inside_the_border() {
        let (tile, covered, _) = background_tile(
            "border: 5px solid black; background-repeat: no-repeat; background-position: 0 0",
        );
        assert_eq!((tile.x, tile.y), (5.0, 5.0));
        assert_eq!(covered, Rect::new(5.0, 5.0, 20.0, 10.0));
    }

    /// A sticky heading rides with the page, stops at its inset, and is carried off
    /// the top when its section runs out.
    #[test]
    fn a_sticky_box_stops_at_its_inset_and_leaves_with_its_container() {
        let green = Color::from_rgb8(0, 128, 0);
        // The heading starts 60px down its section, so at rest it is below its
        // inset and has nothing to stick to yet.
        let html = "<style>body { margin: 0 }                     section { height: 400px; padding-top: 60px }                     h2 { position: sticky; top: 10px; height: 30px; margin: 0;                     background: rgb(0, 128, 0) }                     p { height: 300px; margin: 0 }</style>                    <section><h2>one</h2><p>body</p></section>                    <section><h2>two</h2><p>body</p></section>";

        let heading_tops = |scroll: f32| -> Vec<f64> {
            styled_page(html, scroll)
                .items()
                .iter()
                .filter_map(|item| match item {
                    DisplayItem::Fill { brush, shape, .. } if *brush == Brush::Solid(green) => {
                        Some(shape.bounding_box().y0)
                    }
                    _ => None,
                })
                .collect()
        };

        assert_eq!(heading_tops(0.0).first().copied(), Some(60.0), "at rest");
        assert_eq!(
            heading_tops(100.0).first().copied(),
            Some(10.0),
            "held at its inset while its section is still under it"
        );
        // Far enough down that the first section has nearly gone: the heading is
        // pushed back off the top rather than following the reader forever.
        assert!(
            heading_tops(430.0).first().copied().expect("the heading") < 10.0,
            "its container ran out and did not take it with it"
        );
    }

    /// A fixed box stays on screen while the page moves under it — which is the
    /// whole of what `position: fixed` is for.
    #[test]
    fn a_fixed_box_does_not_move_when_the_page_scrolls() {
        let html = "<style>body { margin: 0 }                     .bar { position: fixed; top: 10px; left: 0; width: 100px; height: 20px;                     background: rgb(255, 0, 0) }                     p { height: 400px }</style>                    <div class=bar>bar</div><p>tall</p><p>tall</p>";

        let top_of = |list: &DisplayList| {
            list.items()
                .iter()
                .find_map(|item| match item {
                    DisplayItem::Fill { brush, shape, .. }
                        if *brush == Brush::Solid(Color::from_rgb8(255, 0, 0)) =>
                    {
                        Some(shape.bounding_box().y0)
                    }
                    _ => None,
                })
                .expect("the fixed bar")
        };

        assert_eq!(top_of(&styled_page(html, 0.0)), 10.0);
        assert_eq!(
            top_of(&styled_page(html, 300.0)),
            10.0,
            "it moved with the page"
        );
    }

    /// Where a picture actually lands: the transform is what decides its size, so
    /// this maps the image's own corners through it and checks the rectangle.
    fn image_rect(list: &DisplayList) -> (f64, f64, f64, f64) {
        let item = list
            .items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Image {
                    image, transform, ..
                } => Some((image.width, image.height, *transform)),
                _ => None,
            })
            .expect("an image item");
        let (width, height, transform) = item;
        let origin = transform * otlyra_gfx::kurbo::Point::new(0.0, 0.0);
        let far = transform * otlyra_gfx::kurbo::Point::new(f64::from(width), f64::from(height));
        (origin.x, origin.y, far.x - origin.x, far.y - origin.y)
    }

    /// `object-fit` decides what happens to the picture inside the box layout
    /// gave it, and never what that box is.
    #[test]
    fn object_fit_places_the_picture_inside_its_box() {
        // A box twice as tall as it is wide, and a picture twice as wide as it
        // is tall, so every value has something to do.
        let page = |fit: &str| {
            page_with_image(
                &format!("img {{ width: 100px; height: 200px; object-fit: {fit} }}"),
                (200, 100),
            )
        };

        assert_eq!(
            image_rect(&page("fill")),
            (0.0, 0.0, 100.0, 200.0),
            "stretched to the box, ratio abandoned"
        );
        assert_eq!(
            image_rect(&page("contain")),
            (0.0, 75.0, 100.0, 50.0),
            "as large as fits, centred in what is left"
        );
        assert_eq!(
            image_rect(&page("cover")),
            (-150.0, 0.0, 400.0, 200.0),
            "large enough to cover, and cut off by the box"
        );
        assert_eq!(
            image_rect(&page("none")),
            (-50.0, 50.0, 200.0, 100.0),
            "its own size, centred"
        );
        assert_eq!(
            image_rect(&page("scale-down")),
            (0.0, 75.0, 100.0, 50.0),
            "which here is `contain`, because its own size does not fit"
        );
    }

    /// A picture larger than its box is cut off at the box rather than spilling
    /// over whatever is drawn next — and the rectangle that does the cutting is
    /// in the picture's own pixels, because that is the space the clip is
    /// applied in.
    #[test]
    fn a_picture_that_overflows_its_box_is_clipped_to_it() {
        let list = page_with_image(
            "img { width: 100px; height: 100px; object-fit: none }",
            (200, 100),
        );
        let clip = list
            .items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Image { clip_rect, .. } => *clip_rect,
                _ => None,
            })
            .expect("a clip");
        assert_eq!((clip.x0, clip.x1), (50.0, 150.0), "the middle hundred");
        assert_eq!((clip.y0, clip.y1), (0.0, 100.0), "and all of the height");
    }

    /// `object-position` moves what is left of the picture inside the box, with
    /// the same arithmetic a background's position uses.
    #[test]
    fn object_position_moves_the_picture_in_its_box() {
        let (x, y, width, height) = image_rect(&page_with_image(
            "img { width: 100px; height: 200px; object-fit: contain; \
             object-position: 0 100% }",
            (200, 100),
        ));
        assert_eq!((x, y), (0.0, 150.0), "at the bottom rather than the middle");
        assert_eq!((width, height), (100.0, 50.0));
    }

    /// A picture asked for at a size is drawn at that size, whatever size its file
    /// is: the scale in the transform is the only thing that decides it.
    #[test]
    fn a_picture_is_drawn_at_the_size_the_page_asked_for() {
        let (x, _, width, height) = image_rect(&page_with_image(
            "img { width: 200px; height: 100px }",
            (64, 64),
        ));
        assert_eq!(x, 0.0);
        assert_eq!((width, height), (200.0, 100.0));

        let (_, _, width, height) = image_rect(&page_with_image("img { width: 320px }", (160, 80)));
        assert_eq!((width, height), (320.0, 160.0), "the ratio is kept");

        let (_, _, width, height) = image_rect(&page_with_image("", (48, 24)));
        assert_eq!((width, height), (48.0, 24.0), "and its own size otherwise");
    }

    fn ops(list: &DisplayList) -> Vec<PaintOp> {
        let mut painter = RecordingPainter::new();
        render(list, &mut painter);
        painter.take()
    }

    #[test]
    fn a_page_paints_its_background_first_and_then_its_text() {
        let ops = ops(&page("<body><p>hello", 0.0));
        assert!(matches!(ops.first(), Some(PaintOp::Fill { .. })));
        assert!(
            ops.iter()
                .any(|op| matches!(op, PaintOp::DrawGlyphs { .. })),
            "the text has to reach the seam"
        );
    }

    #[test]
    fn scrolling_moves_the_text_up_by_exactly_the_scroll_offset() {
        let unscrolled = ops(&page("<body><p>hello", 0.0));
        let scrolled = ops(&page("<body><p>hello", 5.0));

        let y = |ops: &[PaintOp]| {
            ops.iter()
                .find_map(|op| match op {
                    PaintOp::DrawGlyphs { transform, .. } => Some(transform.as_coeffs()[5]),
                    _ => None,
                })
                .expect("some text")
        };
        assert!((y(&unscrolled) - y(&scrolled) - 5.0).abs() < 0.01);
    }

    #[test]
    fn a_link_is_painted_in_the_ua_stylesheets_blue() {
        let ops = ops(&page("<body><p><a>link</a>", 0.0));
        let PaintOp::DrawGlyphs { brush, .. } = ops
            .iter()
            .find(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
            .expect("the link text")
        else {
            unreachable!("filtered above")
        };
        assert_eq!(*brush, Brush::Solid(Color::from_rgb8(0, 0, 0xee)));
    }

    #[test]
    fn off_screen_content_produces_no_items() {
        let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(400);
        let all = page(&html, 0.0);
        // A screenful is some tens of items; four hundred paragraphs would be an
        // order of magnitude more.
        assert!(
            all.len() < 100,
            "only the visible paragraphs should be painted, got {} items",
            all.len()
        );
    }

    #[test]
    fn an_empty_document_still_paints_the_canvas() {
        let list = page("", 0.0);
        let ops = ops(&list);
        assert_eq!(ops.len(), 1, "one fill and nothing else to draw");
        assert!(matches!(ops[0], PaintOp::Fill { .. }));

        // The empty `<html>` and `<body>` are still hit-testable — a click on blank
        // space lands on the document, not on nothing.
        assert!(
            list.items()
                .iter()
                .any(|item| matches!(item, DisplayItem::HitTest { .. }))
        );
    }

    /// Every text run is its own target. A link that is clickable across the whole
    /// line it happens to sit on is worse than no hit testing.
    #[test]
    fn each_text_run_gets_its_own_target() {
        let list = page("<body><p>before <a href=\"/x\">link</a> after", 0.0);
        let targets: Vec<_> = list
            .items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::HitTest { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();

        // html, body, p, and one per run.
        assert!(targets.len() >= 6, "got {} targets", targets.len());
        let runs: Vec<_> = targets.iter().filter(|rect| rect.width() < 700.0).collect();
        assert!(runs.len() >= 3, "one target per run on the line");
        for pair in runs.windows(2) {
            assert!(
                pair[1].x0 >= pair[0].x1 - 0.5,
                "run targets must not overlap: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }
}

#[cfg(test)]
mod border_tests {
    use otlyra_css::cascade::{Viewport as StyleViewport, style_document};
    use otlyra_layout::{Viewport, build_styled_box_tree, layout};
    use otlyra_text::TextEngine;

    use super::*;

    /// The display list for a styled document, at a fixed viewport.
    fn list_for(html: &str) -> DisplayList {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styles = style_document(&document, StyleViewport::default());
        let boxes = build_styled_box_tree(&document, &styles);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
            &mut text,
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );
        build_display_list(&fragments, (800.0, 600.0), 0.0)
    }

    /// Every rectangle filled in `colour`, as (x0, y0, x1, y1).
    ///
    /// By colour, because a page always paints a canvas and a body background too,
    /// and a test about borders should not count them.
    fn rects(list: &DisplayList, colour: Color) -> Vec<[f64; 4]> {
        list.items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Fill {
                    shape,
                    brush: Brush::Solid(fill),
                    ..
                } if *fill == colour => {
                    let bounds = shape.bounding_box();
                    Some([bounds.x0, bounds.y0, bounds.x1, bounds.y1])
                }
                _ => None,
            })
            .collect()
    }

    const RED: Color = Color::from_rgb8(255, 0, 0);
    const BLUE: Color = Color::from_rgb8(0, 0, 255);

    /// Four sides, four rectangles, each the width it was asked for.
    #[test]
    fn each_border_side_is_painted_at_its_own_width() {
        let list = list_for(
            "<style>body { margin: 0 } div { border-top: 4px solid red; \
             border-left: 10px solid blue }</style><div>text</div>",
        );
        let top = rects(&list, RED);
        assert_eq!(top.len(), 1, "expected one red side, got {top:?}");
        assert_eq!(top[0], [0.0, 0.0, 800.0, 4.0]);

        let left = rects(&list, BLUE);
        assert_eq!(left.len(), 1, "expected one blue side, got {left:?}");
        assert_eq!(left[0][0], 0.0);
        assert_eq!(left[0][2], 10.0);
    }

    /// A border whose style is `none` is zero wide however wide it was declared,
    /// so nothing is drawn and nothing moves.
    #[test]
    fn a_border_with_no_style_paints_nothing() {
        let list = list_for(
            "<style>body { margin: 0 } div { border: 10px none red }</style><div>text</div>",
        );
        assert!(
            rects(&list, RED).is_empty(),
            "a border with no style was painted"
        );
    }

    /// A run inside a block carries that block's style. Painting a border from it
    /// would frame the text rather than the box — twice over, once per line.
    #[test]
    fn a_blocks_border_is_painted_once_and_not_around_its_text() {
        let list = list_for(
            "<style>body { margin: 0 } p { border: 2px solid red }</style>\
             <p>a line of text long enough to be its own run</p>",
        );
        let sides = rects(&list, RED);
        assert_eq!(sides.len(), 4, "expected four sides, got {sides:?}");
    }
}
