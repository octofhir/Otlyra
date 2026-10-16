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
//! - [`build_display_list`] — the entry point, and the walk over the fragment
//!   tree that decides what is drawn in what order.
//! - `effects` — the groups a stacking context is composited through.
//! - `background`, `gradient`, `border`, `shadow` and `shape` — what is drawn
//!   for a box: its layers, its edges and the rounded outline both follow.
//! - `scrollbar` — where a scroll port's thumb is, for drawing it and for
//!   deciding what a press landed on.
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
mod spatial;
mod text;
mod widget;

pub use scrollbar::{scrollbar_thumb, scrollbar_travel};
pub use spatial::{PAGE, PAGE_THUMB, PageList, Spatials};

use otlyra_gfx::kurbo::{Affine, Rect as KurboRect, Shape};
use otlyra_gfx::peniko::{BlendMode, Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList, HitTestId, SpatialId};
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
/// `scroll_y`, at `viewport` logical size, as plain geometry.
pub fn build_display_list(tree: &FragmentTree, viewport: (f32, f32), scroll_y: f32) -> DisplayList {
    build_display_list_with(
        tree,
        &Frame {
            viewport,
            scroll_y,
            ..Frame::default()
        },
    )
    .at(scroll_y)
}

/// Where a background picture is found, by the address a style names.
/// A background or mask picture by its address: its pixels, and how many of
/// them go to one of its own (more than one for a vector picture drawn for a
/// dense screen).
pub type BackgroundLookup<'a> = &'a dyn Fn(&str) -> Option<(otlyra_gfx::peniko::ImageData, f32)>;

/// Everything a frame needs beyond the fragments themselves.
///
/// A struct rather than four more arguments: what a frame is made of grows, and a
/// caller that wants only the scroll offset should not have to know about the rest.
pub struct Frame<'a> {
    /// The size of the visible area, in logical pixels.
    pub viewport: (f32, f32),
    /// How far down the page the reader is.
    pub scroll_y: f32,
    /// How much of the page past each edge of the viewport to draw as well.
    ///
    /// The list is in page coordinates and the scroll is applied when it is
    /// drawn, so a list built with a margin serves every scroll that keeps the
    /// viewport inside it — which is most of them.
    pub margin: f32,
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
            margin: 0.0,
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
    let page = build_display_list_with(
        tree,
        &Frame {
            viewport,
            scroll_y,
            ..Frame::default()
        },
    );
    page.list
        .with_offsets(&page.spatials.offsets(scroll_y, port_offset))
}

/// Build the display list for one frame, in page coordinates, with what moves
/// each part of it.
///
/// Everything that scrolls with the page is drawn where layout put it and tagged
/// [`PAGE`]; a fixed box is drawn where it is on the screen and tagged
/// [`SpatialId::VIEWPORT`]; a sticky box gets an id of its own. The page is
/// drawn for the viewport and [`Frame::margin`] past each edge of it, so the list
/// serves every scroll that keeps the viewport inside that.
pub fn build_display_list_with(tree: &FragmentTree, frame: &Frame<'_>) -> PageList {
    let (viewport, scroll_y) = (frame.viewport, frame.scroll_y);
    let _span = tracing::info_span!("build_display_list").entered();
    let (width, height) = viewport;
    let mut list = DisplayList::new();
    let mut spatials = Spatials::new(height);

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

    // The part of the page drawn, in page coordinates, and the screen for what is
    // fixed to it.
    let scrolled = Rect::new(
        0.0,
        scroll_y - frame.margin,
        width,
        height + frame.margin * 2.0,
    );
    // Under all of it, and scrolling with it: the canvas is one colour however
    // far it reaches, so drawing it across the band rather than across the
    // screen draws the same pixels — and a canvas that moves with the page is
    // one a scroll can move rather than paint.
    list.set_spatial(PAGE);
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(canvas),
        brush_transform: None,
        shape: KurboRect::new(
            0.0,
            f64::from(scrolled.y),
            f64::from(width),
            f64::from(scrolled.bottom()),
        )
        .to_path(PATH_TOLERANCE),
    });
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
            close(
                groups.pop().expect("just looked"),
                &mut list,
                frame.background,
            );
        }
        // What this fragment moves with. A fixed box is on the screen already; a
        // sticky one answers to its own rule; what is inside a scroll port moves
        // with the port; everything else is the page.
        let spatial = if fragment.fixed {
            SpatialId::VIEWPORT
        } else if let Some(sticky) = fragment.sticky {
            spatials.sticky(sticky, fragment.scroll_port)
        } else if let Some(port) = fragment.scroll_port {
            spatials.port(port)
        } else {
            PAGE
        };
        // The edge a port cuts its contents off at is the port's box, which moves
        // with the page and not with what scrolls inside it.
        let clip_spatial = if fragment.scroll_port.is_some() && !fragment.fixed {
            PAGE
        } else {
            spatial
        };
        if matches!(fragment.kind, FragmentKind::Box) {
            let faded = fragment.style.opacity < 1.0;
            // A mask cuts the box and everything in it down to the alpha of its
            // pictures, which is done to the group as a whole once it is drawn.
            let masked = fragment
                .style
                .masks
                .iter()
                .any(|layer| layer.image.is_some());
            // About the box's own origin, where the box is drawn — in the
            // coordinates of what it moves with, which is where its items are.
            let moved = transform_of(fragment);
            if faded || masked || moved.is_some() {
                // What an ancestor cuts a transformed box off at is outside the
                // transform: the edge stays where the ancestor is while the box
                // moves under it. Pushed here, before the group, so moving what
                // the group drew does not move it; the box and what is inside it
                // do not push it again.
                let outer_clip = moved.and(fragment.clip);
                if let Some(clip) = outer_clip {
                    list.set_spatial(clip_spatial);
                    list.push(DisplayItem::PushLayer {
                        blend: BlendMode::default(),
                        alpha: 1.0,
                        transform: Affine::IDENTITY,
                        clip: KurboRect::new(
                            f64::from(clip.x),
                            f64::from(clip.y),
                            f64::from(clip.right()),
                            f64::from(clip.bottom()),
                        )
                        .to_path(PATH_TOLERANCE),
                    });
                }
                if faded || masked {
                    // The group's bound is the screen, and the screen does not
                    // scroll.
                    list.set_spatial(SpatialId::VIEWPORT);
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
                    layer: faded || masked,
                    masked: masked.then_some(spatial),
                    transform: moved,
                    outer_clip,
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
        list.set_spatial(spatial);
        // The highlight goes under the run it covers, so the letters are drawn over
        // it rather than through it.
        if matches!(fragment.kind, FragmentKind::Text(_)) {
            for (rect, highlight) in frame.highlights {
                let covered = rect.intersection(&fragment.rect);
                if covered.width <= 0.0 || covered.height <= 0.0 {
                    continue;
                }
                let moved = covered;
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
        // Inside a transformed box, the part of the clip that is its ancestors'
        // was pushed outside the transform with the group; a fragment cut off
        // at nothing more is not cut off again, inside it, where the edge would
        // move with the box.
        let outer = groups.iter().rev().find_map(|group| group.outer_clip);
        let clip = fragment.clip.filter(|clip| Some(*clip) != outer);
        paint(
            fragment,
            clip,
            (clip_spatial, spatial),
            is_root_element,
            frame.background,
            &mut list,
        );
    }

    while let Some(group) = groups.pop() {
        close(group, &mut list, frame.background);
    }

    // Scrollbars last, over everything: the page's, and one for each port that has
    // more to show than it can.
    if frame.scrollbars {
        // The page's own thumb, drawn where it sits with the page at the top and
        // carried down by its own rule as the page scrolls.
        let area = Rect::new(0.0, 0.0, width, height);
        list.set_spatial(PAGE_THUMB);
        paint_scrollbar(&mut list, area, tree.content_height(), 0.0);
        spatials.thumb(area, tree.content_height());
        // A port's bar is on the port, which scrolls with the page, and its
        // thumb is drawn where it sits with the port at its start and carried
        // along the bar as the port scrolls.
        for port in &tree.scroll_ports {
            if !port.port.intersects(&scrolled) {
                continue;
            }
            list.set_spatial(spatials.port_thumb(port.id, port.port, port.content_height));
            paint_scrollbar(&mut list, port.port, port.content_height, 0.0);
        }
    }

    // The caret last, so it is over everything the page drew: a caret behind a
    // background is a caret nobody can see.
    list.set_spatial(PAGE);
    if let Some(caret) = frame.caret {
        let y = caret.y;
        if caret.bottom() >= scrolled.y && caret.y <= scrolled.bottom() {
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
    PageList {
        list,
        spatials,
        canvas,
    }
}

/// The content box inside a border box, by the style's own edges.
fn content_box_of(rect: Rect, style: &otlyra_css::ComputedStyle) -> Rect {
    let length = |value: &otlyra_css::Length| value.resolve(0.0);
    let left = length(&style.padding.left) + style.border.left.width;
    let right = length(&style.padding.right) + style.border.right.width;
    let top = length(&style.padding.top) + style.border.top.width;
    let bottom = length(&style.padding.bottom) + style.border.bottom.width;
    Rect::new(
        rect.x + left,
        rect.y + top,
        (rect.width - left - right).max(0.0),
        (rect.height - top - bottom).max(0.0),
    )
}

/// One fragment's own drawing, where layout put it, tagged with what it moves
/// with. Children are visited by the caller's walk, so this never recurses — a
/// fragment whose parent was culled may still be visible.
///
/// `spatials` is what the edge an ancestor cuts it off at moves with, and what
/// the fragment itself moves with: different for a fragment inside a scroll
/// port, whose edge is the port's box and moves with the page.
fn paint(
    fragment: &Fragment,
    clip: Option<Rect>,
    (clip_spatial, spatial): (SpatialId, SpatialId),
    background_on_canvas: bool,
    background_picture: Option<BackgroundLookup<'_>>,
    list: &mut DisplayList,
) {
    let rect = fragment.rect;

    // Whatever an ancestor cuts this fragment off at, as a layer around its own
    // drawing. One layer per fragment rather than one around a subtree, because the
    // walk is flat: a fragment carries the rectangle it is cut off at, so the two
    // cannot disagree about where the edge is.
    // Every fragment inside a clipping box is clipped, without asking whether it
    // needs to be. It was asked once — whether the fragment fits inside the
    // rectangle — and that was wrong the moment the box scrolled: a fragment that
    // fits where the flow put it does not fit once it has been moved, and the
    // answer was computed before the move.
    if let Some(clip) = clip {
        list.set_spatial(clip_spatial);
        list.push(DisplayItem::PushLayer {
            blend: otlyra_gfx::peniko::BlendMode::default(),
            alpha: 1.0,
            transform: Affine::IDENTITY,
            clip: KurboRect::new(
                f64::from(clip.x),
                f64::from(clip.y),
                f64::from(clip.right()),
                f64::from(clip.bottom()),
            )
            .to_path(PATH_TOLERANCE),
        });
    }
    list.set_spatial(spatial);
    let origin = Affine::translate((f64::from(rect.x), f64::from(rect.y)));

    // Hit testing is a display list too, emitted into the same sequence as the
    // painting it belongs to. Keeping them together is what stops a link from being
    // clickable somewhere other than where it is drawn.
    if let Some(box_id) = fragment.box_id
        && !matches!(fragment.kind, FragmentKind::Line { .. })
    {
        list.push(DisplayItem::HitTest {
            rect: KurboRect::new(
                f64::from(rect.x),
                f64::from(rect.y),
                f64::from(rect.right()),
                f64::from(rect.bottom()),
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
                    shape: shape_with_radii(cast, &fragment.style, shadow.spread),
                });
            }

            // A control that is still a widget is drawn as one, and what it draws
            // *is* its background and its border — so the two the cascade computed
            // are not drawn on top of it. Both references do the same, which is why
            // a themed control ignores the user-agent border it also computes.
            let themed = match &fragment.widget {
                Some(control) => {
                    let painted = rect;
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
                    shape: box_shape(rect, &fragment.style),
                });
            }

            // The layers, bottom-most first: CSS writes them topmost first and
            // paints them in the reverse of that, over the colour.
            if !themed && !background_on_canvas {
                for layer in fragment.style.backgrounds.iter().rev() {
                    paint_background_layer(list, layer, fragment, rect, background_picture);
                }
            }

            // Over everything the box painted and under its border, which is where
            // CSS puts an inset shadow: it belongs to the hole rather than to the
            // box, so a background shows *through* it rather than over it.
            paint_inset_shadows(list, fragment, rect);

            if !themed {
                paint_borders(list, fragment, rect);
            }
        }

        FragmentKind::Line { .. } => {}

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
            text::paint_text_run(list, fragment, run, rect, origin);
        }

        FragmentKind::Text(_) => {}
    }

    if clip.is_some() {
        list.push(DisplayItem::PopLayer);
    }
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

    let position = &style.object_position;
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
