//! One document on screen: box tree, layout, and where the reader has scrolled to.
//!
//! Everything below the shell is a pure function of the step before it — DOM to box
//! tree to fragment tree to display list — so this type holds only what is not a
//! function of the document: the scroll offset, and the cached results of the steps
//! a scroll does not invalidate.
//!
//! What that buys: scrolling relays out nothing and reshapes nothing; it rebuilds
//! the display list, which is a walk over the fragments that are actually visible.
//! A resize invalidates layout, because layout is a function of the width.

mod controls;
mod field;
mod find;
mod focus;
mod pointer;
mod scroll;
mod selection;

pub use controls::{ControlFacts, FileRequest, Numeric, SliderMotion};
pub use field::EditAction;

use otlyra_css::cascade::StyleSources;
use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::{BoxId, BoxTree, Damage, FragmentTree, Images};
use otlyra_text::TextEngine;

use find::Found;
use scroll::Drag;

/// A parsed document, laid out and painted.
pub struct PageScene {
    /// The document's address: what it was fetched from, after any redirect.
    ///
    /// The authority everything the page fetches is asked for on — a base
    /// element can point its addresses elsewhere, never lend it the right to
    /// read the disk — and where its base URL starts.
    url: url::Url,
    /// The document base URL (HTML §2.4.1): what the addresses in its markup
    /// resolve against. Worked out again whenever script changes the tree, which
    /// is when a base element can have come or gone.
    base: url::Url,
    /// The base URL the document's style is resolved against, and the
    /// stylesheets its `<link>` elements and their `@import`s asked for, already
    /// fetched. Kept because a restyle needs them again and a restyle must not
    /// wait on a network.
    sources: StyleSources,
    /// The pictures its `<img>` elements asked for, already decoded. Kept for the
    /// same reason as the sheets: rebuilding the box tree must not wait on a
    /// network either.
    images: Images,
    /// Which file each of those pictures came from, and the density it was chosen
    /// for.
    ///
    /// Which of the several an element offers is a question about the window, and
    /// a window changes: kept so that a resize can put the question again and
    /// find out that the answer is now a different file. Only elements whose
    /// picture has arrived are in here — one that never loaded belongs to the
    /// load that asked for it.
    picture_sources: std::collections::HashMap<NodeId, (String, f32)>,
    /// The document itself, kept because a click resolves to a box, a box to a
    /// node, and a node's attributes are what say where a link goes.
    document: Document,
    boxes: BoxTree,
    /// The last frame's hit-test targets, in paint order, in window coordinates.
    ///
    /// Extracted from the display list rather than kept as a second structure: the
    /// list is what was drawn, so a target taken from it cannot describe a place
    /// nothing was painted.
    targets: Vec<(otlyra_gfx::kurbo::Rect, BoxId)>,
    /// The last layout, and the width it was made at.
    layout: Option<(f32, FragmentTree)>,
    /// Whether that layout has to be done again before the next frame.
    ///
    /// Kept apart from the layout itself, because *out of date* and *not there* are
    /// different answers to different questions. Everything that asks where
    /// something is — a press, a drag, how far the page can scroll — is asking
    /// about the frame the reader is looking at, and that frame is the last one
    /// laid out. Throwing the layout away on every state change left those
    /// questions with no answer at all: a press with nothing to hit-test put the
    /// caret at the end of the field, and a scroll with no content height clamped
    /// the page to the top.
    layout_stale: bool,
    /// The parsed stylesheets and the cascade machinery over them.
    ///
    /// Kept rather than rebuilt, so a resize does not re-parse a page's CSS. Absent
    /// until the first frame, because parsing is not worth doing for a page nobody
    /// has looked at.
    styler: Option<otlyra_css::cascade::Styler>,
    /// Whether the styles the box tree was built from still hold.
    styled: bool,
    /// What the cascade produced, kept past the box tree it built.
    ///
    /// The box tree carries our own `ComputedStyle`, which is the values and not
    /// where they came from. Answering *which rule set this* needs the engine's
    /// own computed values, because the chain of declarations that won hangs off
    /// them — so they are kept rather than dropped once the boxes exist.
    styled_document: Option<otlyra_css::cascade::StyledDocument>,
    /// The reader's default font size, as a multiple of the specification's.
    text_scale: f32,
    /// The palette the environment is asking pages for.
    color_scheme: otlyra_css::cascade::ColorScheme,
    /// How far down the page the reader is, in logical pixels.
    scroll: f32,
    /// The scrollbar the pointer is holding, if it is holding one.
    drag: Option<Drag>,
    /// Whether scrollbars are drawn.
    scrollbars: bool,
    /// Pictures behind boxes, by the address the style names.
    ///
    /// A background is named by a rule rather than by the markup, so what a page
    /// wants is only known once it has been styled — which is after it is first
    /// shown. They arrive late and the page is painted again.
    background_pictures: std::collections::HashMap<String, otlyra_gfx::peniko::ImageData>,
    /// How far each scrollable box inside the page has been scrolled.
    ///
    /// Kept here rather than on the fragment tree, which is rebuilt by every
    /// layout: where the reader had got to inside a panel must survive a resize.
    port_scroll: std::collections::HashMap<BoxId, f32>,
    /// The last frame's content height, so a scroll can be clamped without waiting
    /// for the next one.
    viewport_height: f32,
    /// What the reader has selected, if anything.
    ///
    /// A place in the text rather than a rectangle on the screen: the same
    /// rectangle means different words once the page has been laid out again.
    selection: Option<otlyra_layout::Selection>,
    /// The place in the text that was at the top of the window, to be put back
    /// there once the page has been laid out again.
    ///
    /// A relayout moves everything: the same offset in pixels points at
    /// different words once the lines have broken elsewhere, which is what a
    /// zoom does to a page a reader is half way down. A position in the text
    /// survives it, the way a selection does.
    anchor: Option<otlyra_layout::TextPosition>,
    /// What the reader is looking for on the page, and where it is.
    ///
    /// The query is kept beside the answers because the answers do not survive a
    /// relayout: a match is a pair of positions and a position is a run's number,
    /// which is a number in one layout. So the page is searched again whenever it
    /// is laid out again, rather than the matches being carried over.
    find: Option<Found>,
    /// What the next frame has to redo.
    damage: Damage,
    /// The last list built, and what it was built from.
    ///
    /// The page's half of W10, and the thing `Damage` was written for: every
    /// mutation on this type already records at least `PAINT`, and until now
    /// `build_display_list` took that damage and threw it away. It is read now.
    painted: Option<(Painted, std::sync::Arc<DisplayList>)>,
    /// How many lists have been built rather than reused.
    builds: u64,
    /// What the reader has made the page's controls hold.
    form: otlyra_dom::FormState,
    /// Where the pointer and the focus are.
    interaction: otlyra_css::state::Interaction,
    /// Where the caret sits in the focused field, as a byte offset into its value.
    caret: usize,
    /// Where a selection inside the focused field started, if one is being made.
    ///
    /// A field's selection is a pair of offsets into what the control holds rather
    /// than a place in the page's text: the two are counted in different things,
    /// and a field showing a placeholder is showing text that is in no control at
    /// all.
    field_anchor: Option<usize>,
    /// Whether the pointer is drawing a selection inside a field.
    field_dragging: bool,
    /// What part of the page the last build changed, in page coordinates, when
    /// the build changed only that part.
    ///
    /// `None` means *all of it*, which is the honest answer for anything but a
    /// contained relayout: a restyle can move any box on the page.
    dirty: Option<otlyra_layout::Rect>,
    /// A field whose text changed and which can be re-shaped on its own.
    ///
    /// Set where the value changes and answered at the next frame, because
    /// re-shaping needs the font engine and an event does not carry one. `None`
    /// means there is nothing to re-shape — either nothing was typed, or what was
    /// typed needs the page styled and laid out again.
    retype: Option<NodeId>,
    /// The slider the pointer is dragging, if it is dragging one.
    ///
    /// A slider follows the pointer wherever it goes once it has been taken hold
    /// of, the way every other control that is dragged does — the pointer is
    /// allowed to leave the track and the thumb still follows its horizontal.
    sliding: Option<NodeId>,
    /// Which part of a date or a time field the reader is on.
    segment: usize,
    /// How many digits have been typed into that part since it was reached.
    ///
    /// A part takes its digits in the order they are typed — a `1` and then a `2`
    /// in a month is December, not February — and this is what says whether the
    /// next digit joins the last one or starts again.
    segment_typed: usize,
    /// When the caret was last put somewhere, so that its blinking starts from
    /// there rather than from whatever phase it happened to be in.
    ///
    /// A caret that keeps blinking through the typing is a caret that is invisible
    /// exactly when the reader is looking for it, which is why every platform
    /// restarts it on every keystroke.
    caret_since: std::time::Instant,
    /// A form the reader has asked to send, waiting for whoever navigates.
    pending_submit: Option<otlyra_dom::Submission>,
    /// A file picker the reader pressed, waiting for whoever can open a dialogue.
    ///
    /// The page does not open one and must not: which dialogue a reader gets is a
    /// question about the machine the browser is running on, and the answer is not
    /// the same on all of them. So the page asks, and the shell answers — the same
    /// shape a submission takes out of here.
    pending_pick: Option<FileRequest>,
    /// Whether the last thing the reader did was with the keyboard.
    ///
    /// The whole of the `:focus-visible` decision that is not about the element:
    /// a ring is shown after a key and not after a click, except on something that
    /// takes typing — where it is always shown, because a reader who cannot see
    /// where the letters will go cannot type.
    keyboard: bool,
}

/// The smallest rectangle covering both.
fn union(a: otlyra_layout::Rect, b: otlyra_layout::Rect) -> otlyra_layout::Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    otlyra_layout::Rect::new(x, y, right - x, bottom - y)
}

/// Everything a page's display list is a function of, besides the document.
///
/// A value key beside the damage rather than the damage alone. Damage is a
/// claim every mutation has to remember to make, and a claim that is forgotten
/// once shows a stale frame with no way to notice; the things most likely to be
/// forgotten — where the reader has scrolled to, inside the page and inside a
/// panel — are cheap to compare outright. The two together fail safe: either the
/// damage or the key catches a change.
#[derive(Clone, Debug, PartialEq)]
struct Painted {
    width: f32,
    height: f32,
    top: f32,
    scroll: f32,
    scrollbars: bool,
    pictures: usize,
    ports: Vec<(BoxId, f32)>,
    selection: Option<otlyra_layout::Selection>,
    /// How many places the search found and which of them is the current one.
    ///
    /// Both are drawn, and neither is the rectangles themselves: comparing a
    /// hundred matches against a hundred matches every frame to find out that an
    /// idle page is still idle is not worth the two numbers it would prove.
    find: Option<(usize, usize)>,
    /// The field the caret is in and how far into it.
    ///
    /// Moving the caret changes nothing else about the page — no style, no
    /// layout, not a byte of what it holds — so without this the frame would be
    /// reused and an arrow key would move a caret nobody could see move.
    caret: Option<(NodeId, usize)>,
    /// Whether the caret is in the half of its blink where it is drawn.
    caret_shown: bool,
    /// What is selected inside a field.
    field_selection: Option<(NodeId, usize, usize)>,
}

impl Painted {
    /// Whether these two frames differ in nothing but a caret and what a field
    /// has selected inside itself.
    ///
    /// Both live inside one field's box, so a build that differs in no more than
    /// these has changed one rectangle of the page — which is the difference
    /// between a blinking caret costing a field and costing a screen.
    fn same_but_the_caret(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.top == other.top
            && self.scroll == other.scroll
            && self.scrollbars == other.scrollbars
            && self.pictures == other.pictures
            && self.ports == other.ports
            && self.selection == other.selection
            && self.find == other.find
    }

    /// The fields a caret or a field selection names, in either frame.
    fn fields(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.caret
            .map(|(node, _)| node)
            .into_iter()
            .chain(self.field_selection.map(|(node, _, _)| node))
    }
}

impl std::fmt::Debug for PageScene {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PageScene")
            .field("scroll", &self.scroll)
            .field("builds", &self.builds)
            .finish_non_exhaustive()
    }
}

impl PageScene {
    /// A scene showing `document` at `about:blank`, with nothing fetched for it:
    /// a document made from a string, which is at no address.
    pub fn new(document: Document) -> Self {
        Self::at(document, otlyra_css::cascade::about_blank())
    }

    /// A scene showing `document`, fetched from `url`, before anything it asks
    /// for has arrived.
    ///
    /// Its base URL is read off the tree as it stands, which is short of HTML's
    /// resolving each address as its element goes in: see `document_base_url`.
    pub fn at(document: Document, url: url::Url) -> Self {
        let sources = StyleSources {
            base: document_base_url(&document, &url),
            ..StyleSources::default()
        };
        Self::with_resources(
            document,
            url,
            sources,
            Images::default(),
            std::collections::HashMap::new(),
        )
    }

    /// A scene showing `document`, fetched from `url`, with the stylesheets and
    /// pictures it asked for, and which file each of those pictures came from.
    ///
    /// The sheets carry the base they are resolved against, which is the
    /// document's base URL as it was when they were asked for: a sheet already
    /// parsed keeps the addresses it was parsed with.
    pub fn with_resources(
        document: Document,
        url: url::Url,
        sources: StyleSources,
        images: Images,
        picture_sources: std::collections::HashMap<NodeId, (String, f32)>,
    ) -> Self {
        Self {
            base: document_base_url(&document, &url),
            url,
            sources,
            images,
            picture_sources,
            // Nothing is styled until the first frame says how wide the page is,
            // and nothing unstyled has a box: until then the page is its initial
            // containing block and no more.
            boxes: BoxTree::default(),
            document,
            targets: Vec::new(),
            layout: None,
            layout_stale: true,
            styler: None,
            styled: false,
            styled_document: None,
            text_scale: 1.0,
            color_scheme: otlyra_css::cascade::ColorScheme::Light,
            scroll: 0.0,
            port_scroll: std::collections::HashMap::new(),
            drag: None,
            scrollbars: true,
            background_pictures: std::collections::HashMap::new(),
            viewport_height: 0.0,
            selection: None,
            anchor: None,
            find: None,
            damage: Damage::STYLE,
            painted: None,
            builds: 0,
            form: otlyra_dom::FormState::new(),
            interaction: otlyra_css::state::Interaction::none(),
            caret: 0,
            field_anchor: None,
            field_dragging: false,
            dirty: None,
            retype: None,
            sliding: None,
            segment: 0,
            segment_typed: 0,
            caret_since: std::time::Instant::now(),
            pending_submit: None,
            pending_pick: None,
            keyboard: false,
        }
    }

    /// What the next frame has to redo.
    pub fn damage(&self) -> Damage {
        self.damage
    }

    /// The document behind the page.
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The document's address, and the authority what it fetches is asked for
    /// on.
    pub fn url(&self) -> &url::Url {
        &self.url
    }

    /// The document base URL (HTML §2.4.1): what the addresses in its markup
    /// resolve against.
    pub fn base_url(&self) -> &url::Url {
        &self.base
    }

    /// An address in the document's markup, parsed relative to the document
    /// (HTML §2.4.2): absolute, or `None` when it is not a URL at all.
    ///
    /// The query is encoded as UTF-8 whatever the document's encoding, the one
    /// place this differs from the specification's "encoding-parsing".
    pub fn resolve(&self, href: &str) -> Option<String> {
        self.base.join(href).ok().map(String::from)
    }

    /// Let script change the document in place, and note what it did.
    ///
    /// The alternative is taking the document out and building the whole page
    /// again, which is what a load does and what a timer must not: a page with a
    /// `setInterval` would rebuild itself several times a second, and every
    /// scroll position and selection in it would go with each rebuild.
    ///
    /// `changed` is the binding layer's answer to *did script touch the tree*.
    /// Only then is the style and layout it produced out of date.
    pub fn with_document<R>(&mut self, run: impl FnOnce(&mut Document) -> R) -> R {
        run(&mut self.document)
    }

    /// Everything style and layout produced for this document is out of date,
    /// because script rewrote the tree under it — and so may its base URL be.
    ///
    /// The sheets already parsed keep the addresses they were parsed with; the
    /// markup's addresses resolve against the new base from here on.
    pub fn document_changed(&mut self) {
        self.base = document_base_url(&self.document, &self.url);
        self.invalidate_styles();
    }

    /// Take the document and its address back out, to build the page again with
    /// more of what it asked for — a stylesheet that has since arrived, a picture
    /// that has decoded. Parsing it twice would be the alternative, and the bytes
    /// are gone by then.
    pub fn into_document(self) -> (Document, url::Url) {
        (self.document, self.url)
    }

    /// The box tree behind the page.
    pub fn boxes(&self) -> &BoxTree {
        &self.boxes
    }

    /// How far down the page the reader is.
    pub fn scroll(&self) -> f32 {
        self.scroll
    }

    /// Lay the page out for `width`, reusing the last layout if the width has not
    /// changed.
    fn fragments(&mut self, text: &mut TextEngine, width: f32, height: f32) -> &FragmentTree {
        self.restyle_if_needed(width, height);

        let stale = self.layout_stale || !matches!(&self.layout, Some((last, _)) if *last == width);
        if stale {
            self.damage.add(Damage::of(
                otlyra_layout::InvalidationReason::ViewportResized,
            ));
            let tree = otlyra_layout::layout(
                &mut self.boxes,
                text,
                otlyra_layout::Viewport { width, height },
            );
            self.layout = Some((width, tree));
            self.layout_stale = false;
            // Every path to a new layout comes through here, which is what makes
            // this the one place a search has to be asked again — and the one
            // place the reader can be put back where they were.
            self.find_again();
            self.restore_the_reader_s_place();
        }
        &self.layout.as_ref().expect("just laid out").1
    }

    /// Run the cascade for a viewport of `width` by `height` if this viewport can
    /// change what it computed.
    ///
    /// Most resizes cannot: without a media query or a viewport unit, every element
    /// keeps the style it had, and the width a box is laid out at is layout's
    /// business rather than the cascade's. Asking is what turns a resize from a
    /// re-parse and a re-cascade of the whole document into a relayout.
    fn restyle_if_needed(&mut self, width: f32, height: f32) {
        let viewport = otlyra_css::cascade::Viewport {
            width,
            height,
            scale: 1.0,
            text_scale: self.text_scale,
            color_scheme: self.color_scheme,
        };

        let stale = match self.styler.as_mut() {
            Some(styler) => styler.resize(viewport),
            None => {
                self.styler = Some(otlyra_css::cascade::Styler::new(
                    &self.document,
                    viewport,
                    &self.sources,
                ));
                true
            }
        };

        if !stale && self.styled {
            return;
        }

        let styles = self
            .styler
            .as_mut()
            .expect("a styler was just made if there was none")
            .style_with(&self.document, &self.form, self.interaction);
        self.boxes =
            otlyra_layout::build_page_box_tree(&self.document, &styles, &self.images, &self.form);
        self.styled_document = Some(styles);
        self.styled = true;
        self.layout_stale = true;
        self.damage.add(Damage::of(
            otlyra_layout::InvalidationReason::DocumentLoaded,
        ));
    }

    /// Build the display list for a content area `width` by `height` logical pixels
    /// with its top-left at (0, `top`).
    pub fn build_display_list(
        &mut self,
        text: &mut TextEngine,
        width: f32,
        height: f32,
        top: f32,
    ) -> std::sync::Arc<DisplayList> {
        self.viewport_height = height;
        // What a keystroke left to do, done here because it needs the font engine.
        // A field that cannot be re-shaped on its own takes the whole pipeline,
        // which is what invalidating here asks for.
        let retyped = self.retype.take().and_then(|node| {
            let rect = self.apply_retype(node, text);
            if rect.is_none() {
                self.invalidate_styles();
            }
            rect
        });
        let damage = self.damage.take();

        let mut ports: Vec<(BoxId, f32)> =
            self.port_scroll.iter().map(|(id, at)| (*id, *at)).collect();
        ports.sort_by_key(|(id, _)| otlyra_layout::box_id_to_u64(*id));
        let key = Painted {
            width,
            height,
            top,
            scroll: self.scroll,
            scrollbars: self.scrollbars,
            pictures: self.background_pictures.len(),
            ports,
            selection: self.selection,
            find: self
                .find
                .as_ref()
                .map(|found| (found.at.len(), found.current)),
            caret: self.focused_field().map(|node| (node, self.caret)),
            caret_shown: self.caret_blinks() && self.caret_showing(),
            field_selection: self.field_selection(),
        };
        // Nothing has been reported changed and nothing it is drawn from has
        // moved, so the last list is this frame's list. The hit-test targets go
        // with it untouched — they were taken from this very list, so a press
        // still meets what is on screen.
        if damage.is_none()
            && let Some((built, list)) = &self.painted
            && *built == key
        {
            // The same `Arc`, frame after frame: an unchanged page hands back a
            // handle its consumers can compare by pointer and skip re-scaling.
            return std::sync::Arc::clone(list);
        }

        self.builds += 1;
        // What this build changed, for whoever rasterizes it. Only a build that
        // re-shaped a field or moved a caret can name a rectangle: a relayout can
        // move any box on the page, and a scroll moves all of them. Set on the
        // build rather than on the frame, because a frame served from the cache
        // changed nothing at all and the answer still belongs to the list it
        // hands back.
        self.dirty = self.changed_rect(retyped, damage, &key);
        let scroll = self.scroll;
        // Taken before the layout is borrowed: the offsets are a handful of floats,
        // and the alternative is holding a borrow of the page across the walk.
        let ports = self.port_scroll.clone();
        let pictures = self.background_pictures.clone();
        let scrollbars = self.scrollbars;
        // What the caret is *of*, taken before the layout is borrowed: where it
        // lands needs the fragments, and what it belongs to needs the document.
        let caret_of = self.caret_source();
        let in_field = self
            .field_selection()
            .and_then(|(node, from, to)| self.boxes.box_for(node).map(|box_id| (box_id, from, to)));
        self.keep_caret_in_view(caret_of, text, width, height);
        self.keep_choice_in_view(text, width, height);
        // Laid out before anything counted in runs is taken off this page: a
        // relayout renumbers every run, and laying out is what searches the page
        // again. The second call below is the same layout handed back.
        self.fragments(text, width, height);
        let selected = self.selection;
        // The search's answers, taken before the layout is borrowed for the same
        // reason the selection is. Cloned only while a search is on, and a build
        // only happens when something changed.
        let searched = self
            .find
            .as_ref()
            .map(|found| (found.at.clone(), found.current));
        let showing = self.caret_showing();
        let fragments = self.fragments(text, width, height);
        let caret = showing
            .then(|| caret_of.and_then(|source| source.rect(fragments)))
            .flatten();
        // What is washed behind the text, in the order it is drawn: the selection,
        // then every place the search found, then the one the reader is on. Last
        // wins where two of them cover a word, and the one the reader was just
        // taken to is the one they are looking for.
        let mut highlight: Vec<(otlyra_layout::Rect, otlyra_paint::Highlight)> = selected
            .map(|selection| otlyra_layout::selection::rects(fragments, selection))
            .unwrap_or_default()
            .into_iter()
            .map(|rect| (rect, otlyra_paint::Highlight::Selection))
            .collect();
        // A field's own selection, which is counted in what the control holds and
        // so cannot be one of the page's.
        if let Some((box_id, from, to)) = in_field
            && let Some(rect) = otlyra_layout::selection::range_in(fragments, box_id, from, to)
        {
            highlight.push((rect, otlyra_paint::Highlight::Selection));
        }
        if let Some((at, current)) = &searched {
            for (index, rects) in otlyra_layout::selection::rects_all(fragments, at)
                .into_iter()
                .enumerate()
            {
                let wash = if index == *current {
                    otlyra_paint::Highlight::CurrentMatch
                } else {
                    otlyra_paint::Highlight::Match
                };
                highlight.extend(rects.into_iter().map(|rect| (rect, wash)));
            }
        }
        let mut list = otlyra_paint::build_display_list_with(
            fragments,
            &otlyra_paint::Frame {
                viewport: (width, height),
                scroll_y: scroll,
                port_offset: Some(&|id| ports.get(&id).copied().unwrap_or(0.0)),
                background: Some(&|url: &str| pictures.get(url).cloned()),
                scrollbars,
                highlights: &highlight,
                caret,
            },
        );
        if top != 0.0 {
            list.transform(otlyra_gfx::kurbo::Affine::translate((0.0, f64::from(top))));
        }

        self.targets = list
            .items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::HitTest {
                    rect,
                    transform,
                    id,
                } => Some((
                    transform.transform_rect_bbox(*rect),
                    otlyra_layout::box_id_from_u64(id.0),
                )),
                _ => None,
            })
            .collect();

        // Laying out and cascading are part of building *this* list, and both
        // report damage as they go. Clearing after rather than before is what
        // keeps that from being read as a reason to build the next one again.
        self.damage = Damage::NONE;
        let list = std::sync::Arc::new(list);
        self.painted = Some((key, std::sync::Arc::clone(&list)));
        list
    }

    /// How many display lists this page has built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// What a field holds changed. Invalidate as little as the page allows.
    ///
    /// A text field is a *relayout boundary*, which is Blink's name for a box
    /// whose size cannot be changed by what is inside it: its width comes from
    /// `size` or from CSS, and what it holds is clipped and scrolled within it. So
    /// a keystroke moves the glyphs in one box and nothing else on the page —
    /// unless a rule reads the value, which is what the guard below asks.
    ///
    /// The alternative, and what this used to do, is to treat a typed character
    /// like a freshly loaded document: restyle everything, rebuild every box, lay
    /// the whole page out again. On a page of two hundred paragraphs that is seven
    /// milliseconds a keystroke, all of it work on paragraphs nobody touched.
    fn value_changed(&mut self, node: NodeId) {
        if self.can_retype(node) {
            self.retype = Some(node);
            // The display list is rebuilt — the field draws different glyphs — but
            // neither the cascade nor layout runs for anything but that field.
            self.damage.add(Damage::PAINT);
            return;
        }
        self.invalidate_styles();
    }

    /// Whether this field can be re-shaped on its own.
    ///
    /// Every "no" here is a case where a keystroke changes more than the glyphs in
    /// one box: a rule that reads the value restyles the document, a date field
    /// holds parts rather than a string, and a field that has become empty shows a
    /// placeholder or nothing at all — which is a change of shape rather than of
    /// text, and the builder is what decides shape.
    fn can_retype(&mut self, node: NodeId) -> bool {
        use otlyra_dom::form::Control;

        if self.layout.is_none() || self.layout_stale || !self.styled {
            return false;
        }
        if !Control::of(&self.document, node).is_some_and(|control| control.is_text_entry()) {
            return false;
        }
        if self.segments_of(node).is_some() {
            return false;
        }
        if self.shown_text(node).is_none_or(|text| text.is_empty()) {
            return false;
        }
        // A field that cuts off what does not fit cannot have changed a pixel
        // outside itself. One that does not — a page can ask for that — can, and
        // then this is not a boundary at all.
        let clips = self
            .boxes
            .box_for(node)
            .and_then(|id| self.boxes.get(id))
            .is_some_and(|node| node.style.overflow == otlyra_css::style::Overflow::Clip);
        if !clips {
            return false;
        }
        // Asked of the whole document, and almost always no: `:placeholder-shown`,
        // `:valid`, `:in-range` and their neighbours are the only selectors a typed
        // character can move.
        !self
            .styler
            .as_mut()
            .is_none_or(otlyra_css::cascade::Styler::value_changes_style)
    }

    /// The text a field shows, as the box tree builder would generate it.
    ///
    /// One answer, so the fast path cannot write something a rebuild would not.
    fn shown_text(&self, node: NodeId) -> Option<String> {
        if let Some(temporal) = otlyra_dom::form::temporal_display(&self.document, &self.form, node)
        {
            return Some(temporal);
        }
        let held = self.form.value(&self.document, node);
        if held.is_empty() {
            return None;
        }
        Some(held.to_owned())
    }

    /// Re-shape the field marked by [`Self::value_changed`], in the room it holds.
    ///
    /// Answers with the rectangle it changed, or `None` when it could not do it —
    /// the field has no text box to rewrite, an empty one generates none, or
    /// nothing in the fragment tree carries its box — and then the caller falls
    /// back to the whole pipeline.
    ///
    /// The rectangle is the field's content box, which is where the glyphs are and
    /// nowhere else: a field cuts off what does not fit, so what was typed cannot
    /// have changed a pixel outside it. A field that does *not* cut off is not
    /// re-shaped on its own at all — see [`Self::can_retype`].
    fn apply_retype(&mut self, node: NodeId, text: &mut TextEngine) -> Option<otlyra_layout::Rect> {
        let shown = self.shown_text(node)?;
        let box_id = self.boxes.box_for(node)?;
        let content = {
            let (_, tree) = self.layout.as_ref()?;
            otlyra_layout::selection::content_box(tree, box_id)?
        };
        if !otlyra_layout::set_generated_text(&mut self.boxes, box_id, &shown) {
            return None;
        }
        let children = otlyra_layout::relayout_contained(&self.boxes, text, box_id, content);
        let (_, tree) = self.layout.as_mut()?;
        if !tree.replace_contents(box_id, children) {
            return None;
        }
        // Exchanging one box's contents renumbers every run after it, so a search
        // over the old numbering is a search over the wrong words.
        self.find_again();
        Some(content)
    }

    /// What this build changed, in page coordinates, or `None` for all of it.
    ///
    /// Two things can change part of a page and nothing else: a field that was
    /// re-shaped on its own, and a caret — which blinks, moves, and takes a
    /// selection with it, all inside one field's box. Everything else is either a
    /// relayout, which can move any box, or a scroll, which moves every box.
    fn changed_rect(
        &self,
        retyped: Option<otlyra_layout::Rect>,
        damage: Damage,
        key: &Painted,
    ) -> Option<otlyra_layout::Rect> {
        if damage.contains(Damage::LAYOUT) {
            return None;
        }
        let (before, _) = self.painted.as_ref()?;
        if !before.same_but_the_caret(key) {
            return None;
        }
        let (_, tree) = self.layout.as_ref()?;
        // Both frames' fields: a caret that moved from one field to another left
        // one and arrived in the other, and both have to be redrawn.
        let mut rect = retyped;
        for node in before.fields().chain(key.fields()) {
            let content = self
                .boxes
                .box_for(node)
                .and_then(|id| otlyra_layout::selection::content_box(tree, id))?;
            rect = Some(match rect {
                Some(rect) => union(rect, content),
                None => content,
            });
        }
        rect
    }

    /// What the last built list changed, in page coordinates, when it changed only
    /// part of the page. `None` means the whole of it.
    ///
    /// Read by the compositor above: a keystroke that re-shapes one field has no
    /// business re-rasterizing the paragraphs under it.
    pub fn dirty(&self) -> Option<otlyra_layout::Rect> {
        self.dirty
    }

    /// Everything the page was laid out and styled for has changed.
    ///
    /// What a zoom does: the page is laid out in a viewport of a different size,
    /// so the cascade's media queries and viewport units have a different answer
    /// and every box has a different width to fill.
    pub fn invalidate_layout(&mut self) {
        self.invalidate_styles();
    }

    /// Everything the cascade produced is out of date.
    fn invalidate_styles(&mut self) {
        self.styled = false;
        self.layout_stale = true;
        self.damage.add(Damage::of(
            otlyra_layout::InvalidationReason::DocumentLoaded,
        ));
    }

    /// What the reader has asked the default font size to be, as a multiple.
    ///
    /// A restyle when it changes, because it changes what `medium` computes to
    /// and every element that inherited a size inherited that.
    pub fn set_text_scale(&mut self, scale: f32) {
        if (self.text_scale - scale).abs() < f32::EPSILON {
            return;
        }
        self.text_scale = scale;
        self.styled = false;
        self.damage.add(otlyra_layout::Damage::LAYOUT);
    }

    /// Which palette `prefers-color-scheme` answers with.
    ///
    /// The frame is rebuilt, but a restyle happens only if the page asked: the
    /// cascade is told the new scheme and says whether any rule now evaluates
    /// differently, so a page with no `prefers-color-scheme` query keeps every
    /// style it had. The damage is what gets the question asked at all — a
    /// frame with nothing reported changed is served from the last one.
    pub fn set_color_scheme(&mut self, scheme: otlyra_css::cascade::ColorScheme) {
        if self.color_scheme == scheme {
            return;
        }
        self.color_scheme = scheme;
        self.damage.add(otlyra_layout::Damage::LAYOUT);
    }

    /// Change the document, and let the next frame notice.
    ///
    /// The whole of the invalidation an edit needs: the DOM is handed to `edit`,
    /// and whatever it did is followed by a restyle, a fresh box tree and a
    /// relayout. Coarse on purpose — `Damage::STYLE` is the honest answer to *an
    /// attribute changed and we do not know which rules cared*, and a narrower
    /// one would be a claim this cannot yet support. It is the seam that was
    /// missing, not the optimisation.
    ///
    /// Returns whatever `edit` returned, so a caller can say what it did.
    pub fn edit<T>(&mut self, edit: impl FnOnce(&mut otlyra_dom::DocumentMutator<'_>) -> T) -> T {
        let out = edit(&mut otlyra_dom::DocumentMutator::new(&mut self.document));
        // The sheets have not changed, so the styler is kept; what has to go is
        // everything downstream of the document, which is everything else.
        self.styled = false;
        self.styled_document = None;
        self.layout_stale = true;
        self.painted = None;
        self.damage.add(Damage::of(
            otlyra_layout::InvalidationReason::AttributeChanged,
        ));
        out
    }

    /// Which rules set the values on a node, weakest first.
    ///
    /// Empty for a node the cascade was never asked about — a text node, or an
    /// element under `display: none` — which is the same answer the computed
    /// pane gives for one, and for the same reason.
    pub fn rules_for(&self, node: NodeId) -> Vec<otlyra_css::cascade::MatchedRule> {
        let Some(styler) = self.styler.as_ref() else {
            return Vec::new();
        };
        self.styled_document
            .as_ref()
            .and_then(|styled| styled.style_of(node))
            .map(|style| styler.rules_for(style))
            .unwrap_or_default()
    }

    /// The edges layout actually gave a box, if it laid one out.
    ///
    /// The used values. A computed style says `auto` for a margin and only
    /// layout knows what `auto` came out as, so a panel that resolved the
    /// computed style itself would be right about everything except the one
    /// case it was opened to look at.
    pub fn used_edges(&self, id: BoxId) -> Option<otlyra_layout::UsedEdges> {
        self.layout
            .as_ref()?
            .1
            .iter()
            .find(|fragment| fragment.box_id == Some(id))
            .and_then(|fragment| fragment.used)
    }

    /// The background pictures this page names and has not been given.
    ///
    /// Asked for after a frame, because the styles that name them are computed on
    /// the way to one.
    pub fn wanted_pictures(&self) -> Vec<String> {
        let mut wanted: Vec<String> = Vec::new();
        for id in self.boxes.descendants(self.boxes.root()) {
            for layer in &self.boxes.node(id).style.backgrounds {
                let Some(url) = layer.image.as_deref() else {
                    continue;
                };
                if self.background_pictures.contains_key(url) {
                    continue;
                }
                if !wanted.iter().any(|already| already == url) {
                    wanted.push(url.to_owned());
                }
            }
        }
        wanted
    }

    /// The `@font-face` rules the page's stylesheets declare.
    ///
    /// Empty until the page has been styled once, which is where the sheets are
    /// parsed: a rule nobody has read yet names no font.
    pub fn wanted_fonts(&self) -> Vec<otlyra_css::cascade::FontFace> {
        self.styler
            .as_ref()
            .map(|styler| styler.font_faces().to_vec())
            .unwrap_or_default()
    }

    /// A font the page asked for has arrived and been registered.
    ///
    /// Nothing here holds the font — the shaper does — so what this is for is the
    /// frame: every line was measured in whatever family the stack fell back to,
    /// and none of those measurements hold any more.
    pub fn font_arrived(&mut self) {
        self.layout_stale = true;
        self.damage.add(otlyra_layout::Damage::LAYOUT);
    }

    /// Which file an element's picture came from, and the density it was chosen
    /// for. `None` where nothing has arrived for it.
    pub fn picture_source(&self, node: NodeId) -> Option<(&str, f32)> {
        self.picture_sources
            .get(&node)
            .map(|(src, density)| (src.as_str(), *density))
    }

    /// Hand an element the picture it now asks for, in place of the one it has.
    ///
    /// The box tree goes with it: a picture is a box's content, and a different
    /// file is a different intrinsic size — so the page is styled and laid out
    /// again rather than repainted.
    pub fn set_image(&mut self, node: NodeId, src: String, picture: otlyra_layout::Picture) {
        self.picture_sources.insert(node, (src, picture.density));
        self.images.insert(node, picture);
        self.styled = false;
        self.layout_stale = true;
        self.damage.add(Damage::of(
            otlyra_layout::InvalidationReason::DocumentLoaded,
        ));
    }

    /// Hand over a picture the page asked for.
    pub fn set_picture(&mut self, url: String, picture: otlyra_gfx::peniko::ImageData) {
        self.background_pictures.insert(url, picture);
        self.damage.add(Damage::PAINT);
    }
}

/// The document base URL (HTML §2.4.1): the frozen base URL of its first base
/// element with an `href` (§4.2.3), or its own address when there is none.
///
/// The `href` is parsed against the document's address, never against another
/// base. One that does not parse, or that is a `data:` or `javascript:` URL, is
/// refused and the document's own address stands, as "set the frozen base URL"
/// says.
///
/// Read off the tree as it stands, and every address in the page resolved
/// with it. This deliberately stops short of HTML, which resolves an `<img>`'s
/// `src`, a `<link>`'s `href`, a `<style>` sheet, a `style` attribute and a
/// presentational hint when each is inserted or set: there a base element
/// later in the document moves none of the addresses before it, and here it
/// moves them all. A conforming document puts its base before anything with an
/// address in it (§4.2.3), where the two agree; and a link is resolved when it
/// is followed in HTML too.
fn document_base_url(document: &Document, url: &url::Url) -> url::Url {
    document
        .base_element_href()
        .and_then(|href| url.join(href).ok())
        .filter(|base| !matches!(base.scheme(), "data" | "javascript"))
        .unwrap_or_else(|| url.clone())
}

/// The document's `<title>`, if it has one.
///
/// Browser interface rather than page content, which is why it is here and not in
/// the box tree: `<title>` is `display: none`, and the tab still has to be named
/// something.
pub fn title_of(document: &Document) -> Option<String> {
    fn find(document: &Document, id: NodeId) -> Option<String> {
        if let Some(element) = document.get(id).and_then(|node| node.element())
            && element.name.local.as_ref() == "title"
        {
            let mut text = String::new();
            for child in document.children(id) {
                if let Some(NodeData::Text(chunk)) = document.get(child).map(|node| &node.data) {
                    text.push_str(chunk);
                }
            }
            let text = text.trim().to_owned();
            return (!text.is_empty()).then_some(text);
        }
        document
            .children(id)
            .find_map(|child| find(document, child))
    }
    find(document, document.root())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod raster_tests;

/// Every node under `root`, in tree order.
/// Every node under `root`, in the order the markup put them in.
///
/// [`descendants_of`] answers the same set in whatever order the walk happens
/// to pop them, which is fine for *did any of these change* and wrong for
/// traversal: what Tab does next is a question about the order a reader meets
/// things in.
fn in_document_order(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        order.push(node);
        let children: Vec<NodeId> = document.children(node).collect();
        stack.extend(children.into_iter().rev());
    }
    order
}

fn descendants_of(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(document.children(node));
    }
    order
}
