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
//! - `widget` — what a form control looks like, drawn rather than asked of the
//!   operating system.
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

mod background;
mod border;
mod effects;
mod gradient;
mod scrollbar;
mod shadow;
mod shape;
mod widget;

pub use scrollbar::{scrollbar_thumb, scrollbar_travel};

use otlyra_gfx::kurbo::{Affine, Rect as KurboRect, Shape};
use otlyra_gfx::peniko::{BlendMode, Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList, HitTestId};
use otlyra_layout::fragment::{Fragment, FragmentKind, FragmentTree, Rect};

use background::paint_background_layer;
use border::paint_borders;
use effects::{Group, close, transform_of};
use scrollbar::paint_scrollbar;
use shadow::paint_inset_shadows;
use shape::{box_shape, shape_with_radii};

/// Flattening tolerance for shapes entering the display list. Matches the recording
/// backend's, so a display list and its recording agree.
const PATH_TOLERANCE: f64 = 0.1;

/// The caret. The text colour rather than a colour of its own, because a caret is
/// where the next letter goes and it should look like one.
const CARET: Color = Color::from_rgba8(0, 0, 0, 0xff);

/// The colour a selection is drawn in.
///
/// The platform's own highlight is a preference this cannot read yet; this is the
/// blue every browser falls back to, and it is opaque because the text is drawn
/// over it rather than through it.
const SELECTION: Color = Color::from_rgb8(0xB4, 0xD5, 0xFE);

/// The colour every place a search found is washed in.
///
/// Yellow rather than the selection's blue, because the two mean different
/// things and a reader who has both on the page has to be able to tell which is
/// which. Pale, because there may be a hundred of them and a page of bright
/// blocks is a page nobody can read.
const MATCH: Color = Color::from_rgb8(0xFD, 0xE8, 0x8B);

/// And the colour the one the reader is on is washed in.
///
/// The same hue carried further, rather than a different one: it is the same
/// kind of thing as the others and has to be found among them at a glance, which
/// a stronger version of them is and a fourth colour is not.
const CURRENT_MATCH: Color = Color::from_rgb8(0xFF, 0x9C, 0x30);

/// Why a rectangle behind the text is washed.
///
/// Which rectangle means what is a question about the page, so the caller
/// answers it; what each of them looks like is a question about the picture, so
/// this crate answers that. Drawn in the order they are given, so a caller that
/// puts two washes over one word decides which of them is seen.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Highlight {
    /// What the reader has selected.
    Selection,
    /// One of the places a search found.
    Match,
    /// The one of those the reader is on.
    CurrentMatch,
}

impl Highlight {
    /// The colour it is washed in, for whoever has to check that it was.
    pub fn colour(self) -> Color {
        match self {
            Self::Selection => SELECTION,
            Self::Match => MATCH,
            Self::CurrentMatch => CURRENT_MATCH,
        }
    }
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
    /// What is washed behind the text, in page coordinates, and why.
    ///
    /// Behind the text rather than over it, which is what makes the letters still
    /// readable: a highlight over them would tint them, and inverting them
    /// instead is a different tradition that this platform is not in. One layer
    /// for the selection and for a search's matches rather than an overlay each,
    /// because they are the same picture — a colour under a run of letters — and
    /// two of them would be two answers about which is on top.
    pub highlights: &'a [(Rect, Highlight)],
    /// Where the caret is, in page coordinates, if the page has one.
    ///
    /// Over everything rather than under it, unlike the selection: a caret sits
    /// between two letters and has nothing to be behind.
    pub caret: Option<Rect>,
}

impl Default for Frame<'_> {
    fn default() -> Self {
        Self {
            viewport: (0.0, 0.0),
            scroll_y: 0.0,
            port_offset: None,
            background: None,
            scrollbars: true,
            highlights: &[],
            caret: None,
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
            for (rect, highlight) in frame.highlights {
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
                    brush: Brush::Solid(highlight.colour()),
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

    // The caret last, so it is over everything the page drew: a caret behind a
    // background is a caret nobody can see.
    if let Some(caret) = frame.caret {
        let y = caret.y - scroll_y;
        if y + caret.height >= 0.0 && y <= height {
            list.push(DisplayItem::Fill {
                style: Fill::NonZero,
                transform: Affine::IDENTITY,
                brush: Brush::Solid(CARET),
                brush_transform: None,
                shape: KurboRect::new(
                    f64::from(caret.x),
                    f64::from(y),
                    f64::from(caret.x + caret.width.max(1.0)),
                    f64::from(y + caret.height),
                )
                .to_path(PATH_TOLERANCE),
            });
        }
    }

    tracing::debug!(items = list.len(), "display list built");
    list
}

/// The content box inside a border box, by the style's own edges.
fn content_box_of(rect: Rect, style: &otlyra_css::ComputedStyle) -> Rect {
    let length = |value: otlyra_css::Length| value.resolve(0.0);
    let left = length(style.padding.left) + style.border.left.width;
    let right = length(style.padding.right) + style.border.right.width;
    let top = length(style.padding.top) + style.border.top.width;
    let bottom = length(style.padding.bottom) + style.border.bottom.width;
    Rect::new(
        rect.x + left,
        rect.y + top,
        (rect.width - left - right).max(0.0),
        (rect.height - top - bottom).max(0.0),
    )
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
                if shadow.color.components[3] <= 0.0 || shadow.inset {
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

            // A control that is still a widget is drawn as one, and what it draws
            // *is* its background and its border — so the two the cascade computed
            // are not drawn on top of it. Both references do the same, which is why
            // a themed control ignores the user-agent border it also computes.
            let themed = match &fragment.widget {
                Some(control) => {
                    let mut painted = rect;
                    painted.y -= scroll_y;
                    // A colour well fills its content box and nothing else, so
                    // the widget is given both edges: the frame it draws is the
                    // border box, and what it shows is inside the padding.
                    let inner = content_box_of(painted, &fragment.style);
                    widget::paint(list, control, painted, inner)
                }
                None => false,
            };

            let background = fragment.style.background_color;
            // Transparent is the initial value, so most boxes paint nothing at all.
            if !themed && background.components[3] > 0.0 && !background_on_canvas {
                list.push(DisplayItem::Fill {
                    style: Fill::NonZero,
                    transform: Affine::IDENTITY,
                    brush: Brush::Solid(background),
                    brush_transform: None,
                    shape: box_shape(rect, scroll_y, &fragment.style),
                });
            }

            // The layers, bottom-most first: CSS writes them topmost first and
            // paints them in the reverse of that, over the colour.
            if !themed && !background_on_canvas {
                for layer in fragment.style.backgrounds.iter().rev() {
                    paint_background_layer(
                        list,
                        layer,
                        fragment,
                        rect,
                        scroll_y,
                        background_picture,
                    );
                }
            }

            // Over everything the box painted and under its border, which is where
            // CSS puts an inset shadow: it belongs to the hole rather than to the
            // box, so a background shows *through* it rather than over it.
            paint_inset_shadows(list, fragment, rect, scroll_y);

            if !themed {
                paint_borders(list, fragment, rect, scroll_y);
            }
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

/// The colour a shaped run carried, back as a paint colour.
fn brush_to_color(brush: [u8; 4]) -> Color {
    Color::from_rgba8(brush[0], brush[1], brush[2], brush[3])
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod border_tests;
