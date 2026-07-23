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

use otlyra_css::cascade::ExternalSheets;
use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::{BoxId, BoxTree, Damage, FragmentTree, Images, build_box_tree};
use otlyra_text::TextEngine;

/// A parsed document, laid out and painted.
pub struct PageScene {
    /// The stylesheets the document's `<link>` elements asked for, already
    /// fetched. Kept because a restyle needs them again and a restyle must not
    /// wait on a network.
    sheets: ExternalSheets,
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
    /// What the next frame has to redo.
    damage: Damage,
    /// The last list built, and what it was built from.
    ///
    /// The page's half of W10, and the thing `Damage` was written for: every
    /// mutation on this type already records at least `PAINT`, and until now
    /// `build_display_list` took that damage and threw it away. It is read now.
    painted: Option<(Painted, DisplayList)>,
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
    /// When the caret was last put somewhere, so that its blinking starts from
    /// there rather than from whatever phase it happened to be in.
    ///
    /// A caret that keeps blinking through the typing is a caret that is invisible
    /// exactly when the reader is looking for it, which is why every platform
    /// restarts it on every keystroke.
    caret_since: std::time::Instant,
    /// A form the reader has asked to send, waiting for whoever navigates.
    pending_submit: Option<otlyra_dom::Submission>,
    /// Whether the last thing the reader did was with the keyboard.
    ///
    /// The whole of the `:focus-visible` decision that is not about the element:
    /// a ring is shown after a key and not after a click, except on something that
    /// takes typing — where it is always shown, because a reader who cannot see
    /// where the letters will go cannot type.
    keyboard: bool,
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
    /// A scene showing `document`, with nothing fetched for it.
    pub fn new(document: Document) -> Self {
        Self::with_resources(
            document,
            ExternalSheets::default(),
            Images::default(),
            std::collections::HashMap::new(),
        )
    }

    /// A scene showing `document` with the stylesheets and pictures it asked for,
    /// and which file each of those pictures came from.
    pub fn with_resources(
        document: Document,
        sheets: ExternalSheets,
        images: Images,
        picture_sources: std::collections::HashMap<NodeId, (String, f32)>,
    ) -> Self {
        Self {
            sheets,
            images,
            picture_sources,
            boxes: build_box_tree(&document),
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
            damage: Damage::STYLE,
            painted: None,
            builds: 0,
            form: otlyra_dom::FormState::new(),
            interaction: otlyra_css::state::Interaction::none(),
            caret: 0,
            field_anchor: None,
            field_dragging: false,
            caret_since: std::time::Instant::now(),
            pending_submit: None,
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

    /// Take the document back out, to build the page again with more of what it
    /// asked for — a stylesheet that has since arrived, a picture that has decoded.
    /// Parsing it twice would be the alternative, and the bytes are gone by then.
    pub fn into_document(self) -> Document {
        self.document
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
                    &self.sheets,
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
        self.boxes = otlyra_layout::build_page_box_tree(
            &self.document,
            Some(&styles),
            &self.images,
            &self.form,
        );
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
    ) -> DisplayList {
        self.viewport_height = height;
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
            return list.clone();
        }

        self.builds += 1;
        let scroll = self.scroll;
        // Taken before the layout is borrowed: the offsets are a handful of floats,
        // and the alternative is holding a borrow of the page across the walk.
        let ports = self.port_scroll.clone();
        let pictures = self.background_pictures.clone();
        let scrollbars = self.scrollbars;
        let selected = self.selection;
        // What the caret is *of*, taken before the layout is borrowed: where it
        // lands needs the fragments, and what it belongs to needs the document.
        let caret_of = self.caret_source();
        let in_field = self
            .field_selection()
            .and_then(|(node, from, to)| self.boxes.box_for(node).map(|box_id| (box_id, from, to)));
        self.keep_caret_in_view(caret_of, text, width, height);
        self.keep_choice_in_view(text, width, height);
        let showing = self.caret_showing();
        let fragments = self.fragments(text, width, height);
        let caret = showing
            .then(|| caret_of.and_then(|source| source.rect(fragments)))
            .flatten();
        let mut highlight = selected
            .map(|selection| otlyra_layout::selection::rects(fragments, selection))
            .unwrap_or_default();
        // A field's own selection, which is counted in what the control holds and
        // so cannot be one of the page's.
        if let Some((box_id, from, to)) = in_field
            && let Some(rect) = otlyra_layout::selection::range_in(fragments, box_id, from, to)
        {
            highlight.push(rect);
        }
        let mut list = otlyra_paint::build_display_list_with(
            fragments,
            &otlyra_paint::Frame {
                viewport: (width, height),
                scroll_y: scroll,
                port_offset: Some(&|id| ports.get(&id).copied().unwrap_or(0.0)),
                background: Some(&|url: &str| pictures.get(url).cloned()),
                scrollbars,
                selection: &highlight,
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
        self.painted = Some((key, list.clone()));
        list
    }

    /// How many display lists this page has built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// The topmost box at `point`, in window logical coordinates.
    ///
    /// Reads the last frame's targets: a click lands on what the user was looking
    /// at, which is the frame that was on screen, not the one that would be built
    /// now.
    pub fn box_at(&self, x: f64, y: f64) -> Option<BoxId> {
        let point = otlyra_gfx::kurbo::Point::new(x, y);
        self.targets
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, id)| *id)
    }

    /// The element the pointer is over, walking out of the box tree to the nearest
    /// node that has one.
    ///
    /// Text inside a button belongs to an anonymous box, and the button is two
    /// boxes above it — so a press on the word "Send" has to find the button the
    /// same way a click on a link finds the link.
    fn node_at(&self, x: f64, y: f64) -> Option<NodeId> {
        let mut current = self.box_at(x, y);
        while let Some(id) = current {
            let node = self.boxes.get(id)?;
            if let Some(node) = node.node {
                return Some(node);
            }
            current = node.parent;
        }
        None
    }

    /// The control a press at this point acts on, if any.
    ///
    /// A `<label>` acts on the control it names, which is what makes the words
    /// beside a checkbox tick it — and it is the label's own activation behaviour
    /// rather than a special case in the hit test.
    fn control_at(&self, x: f64, y: f64) -> Option<NodeId> {
        let mut current = self.node_at(x, y);
        while let Some(node) = current {
            if otlyra_dom::form::Control::of(&self.document, node).is_some() {
                return Some(node);
            }
            if let Some(labelled) = otlyra_dom::form::labeled_control(&self.document, node) {
                return Some(labelled);
            }
            current = self.document.get(node).and_then(|inner| inner.parent);
        }
        None
    }

    /// Whether a press here lands on a control rather than on the page.
    pub fn control_under(&self, x: f64, y: f64) -> bool {
        self.control_at(x, y).is_some()
    }

    /// Whether an element can take the focus.
    fn is_focusable(&self, node: NodeId) -> bool {
        match otlyra_dom::form::Control::of(&self.document, node) {
            Some(control) => {
                !matches!(
                    control,
                    otlyra_dom::form::Control::Option
                        | otlyra_dom::form::Control::Optgroup
                        | otlyra_dom::form::Control::Output
                        | otlyra_dom::form::Control::Meter
                        | otlyra_dom::form::Control::Progress
                        | otlyra_dom::form::Control::Fieldset
                ) && !otlyra_dom::form::is_disabled(&self.document, node)
            }
            None => false,
        }
    }

    /// Whether the focused control takes typing.
    fn focused_field(&self) -> Option<NodeId> {
        let node = self.interaction.focus?;
        let control = otlyra_dom::form::Control::of(&self.document, node)?;
        (control.is_text_entry() && otlyra_dom::form::is_mutable(&self.document, node))
            .then_some(node)
    }

    /// Adopt a new interaction, restyling only if it can change anything.
    ///
    /// Two reasons it can. A selector may depend on the state — which the engine's
    /// own index answers without looking at a rule — or the element may be a
    /// control, whose widget is drawn from the state whether a rule mentions it or
    /// not. The second is why hovering a button repaints on a page with no `:hover`
    /// rule in it at all.
    fn set_interaction(&mut self, next: otlyra_css::state::Interaction) -> bool {
        let before = self.interaction;
        if before == next {
            return false;
        }
        self.interaction = next;

        let touched = otlyra_css::state::touched_nodes(&self.document, before, next);
        let control_touched = touched
            .iter()
            .any(|&node| otlyra_dom::form::Control::of(&self.document, node).is_some());
        let styled_touched = self.styler.as_mut().is_some_and(|styler| {
            styler.interaction_changes_style(&self.document, &self.form, before, next)
        });
        if !control_touched && !styled_touched {
            return false;
        }
        self.invalidate_styles();
        true
    }

    /// Everything the cascade produced is out of date.
    fn invalidate_styles(&mut self) {
        self.styled = false;
        self.layout_stale = true;
        self.damage.add(Damage::of(
            otlyra_layout::InvalidationReason::DocumentLoaded,
        ));
    }

    /// The pointer moved to this point. Returns whether the page has to be drawn
    /// again.
    pub fn pointer_moved(&mut self, x: f64, y: f64) -> bool {
        // A drag inside a field takes the letters it passes, wherever the pointer
        // wanders — the same rule a drag across the page follows.
        if self.field_dragging
            && let Some(node) = self.focused_field()
            && let Some(box_id) = self.boxes.box_for(node)
            && let Some((_, fragments)) = self.layout.as_ref()
            && let Some(offset) =
                otlyra_layout::selection::offset_in(fragments, box_id, x as f32, y as f32)
        {
            let value_len = self.form.value(&self.document, node).len();
            self.caret = offset.min(value_len);
            self.restart_caret();
            return true;
        }
        let hover = self.node_at(x, y);
        self.set_interaction(otlyra_css::state::Interaction {
            hover,
            ..self.interaction
        })
    }

    /// The pointer left the page.
    pub fn pointer_left(&mut self) -> bool {
        self.set_interaction(otlyra_css::state::Interaction {
            hover: None,
            active: None,
            ..self.interaction
        })
    }

    /// The pointer went down. Returns whether the page has to be drawn again.
    ///
    /// The focus moves on the press rather than on the release, which is what every
    /// platform does and what makes a click-and-drag inside a field select rather
    /// than move the focus somewhere else halfway through.
    pub fn pointer_pressed(&mut self, x: f64, y: f64) -> bool {
        self.pointer_pressed_times(x, y, 1)
    }

    /// The same, told how many presses in a row this is.
    ///
    /// A second press takes the word under it and a third the whole of what the
    /// field holds, which is what a second and a third press mean everywhere else.
    pub fn pointer_pressed_times(&mut self, x: f64, y: f64, clicks: u32) -> bool {
        self.keyboard = false;
        let target = self.control_at(x, y);

        // A press on an option belongs to the list it is in, and the list belongs
        // to the control it hangs off: pressing "Beta" is pressing the drop-down.
        let owning = target.and_then(|node| otlyra_dom::form::owning_select(&self.document, node));
        let mut open = self.interaction.open;
        if let Some(node) = target {
            match otlyra_dom::form::Control::of(&self.document, node) {
                Some(otlyra_dom::form::Control::Select)
                    if otlyra_dom::form::is_mutable(&self.document, node) =>
                {
                    // A press on the control itself opens the list, and a second
                    // one puts it away again.
                    open = (open != Some(node)).then_some(node);
                }
                Some(otlyra_dom::form::Control::Option) => {
                    if let Some(select) = owning
                        && open == Some(select)
                        && !otlyra_dom::form::is_disabled(&self.document, node)
                    {
                        self.choose_option(select, node);
                        open = None;
                    }
                }
                // A press on anything else puts an open list away, which is what
                // pressing away from a menu means everywhere.
                _ => open = None,
            }
        } else {
            open = None;
        }

        let target = owning.or(target);
        let focus = target.filter(|&node| self.is_focusable(node));
        if let Some(node) = focus {
            // Between the two letters the pointer is between, and at the end of
            // what is there when the press landed past it — which is what a click
            // into a field means anywhere else. Only when the field is showing what
            // it holds: a placeholder is not the value, and an offset into one is
            // not an offset into the other.
            let value = self.form.value(&self.document, node).to_owned();
            let landed = (!value.is_empty())
                .then(|| {
                    let box_id = self.boxes.box_for(node)?;
                    let (_, fragments) = self.layout.as_ref()?;
                    otlyra_layout::selection::offset_in(fragments, box_id, x as f32, y as f32)
                })
                .flatten();
            self.caret = landed.unwrap_or(value.len()).min(value.len());
            self.field_anchor = Some(self.caret);
            self.field_dragging = true;
            self.restart_caret();

            match clicks % 3 {
                2 => {
                    let (from, to) = word_around(&value, self.caret);
                    self.field_anchor = Some(from);
                    self.caret = to;
                    self.field_dragging = false;
                }
                0 if clicks > 0 => {
                    self.field_anchor = Some(0);
                    self.caret = value.len();
                    self.field_dragging = false;
                }
                _ => {}
            }
        }
        // A ring after a press only where the reader is going to type: that is the
        // one case the specification says to show it whatever the pointer did.
        let focus_visible = focus.is_some_and(|node| {
            otlyra_dom::form::Control::of(&self.document, node)
                .is_some_and(otlyra_dom::form::Control::is_text_entry)
        });
        self.set_interaction(otlyra_css::state::Interaction {
            active: target,
            focus,
            focus_visible,
            open,
            ..self.interaction
        })
    }

    /// Make one option of a `<select>` the chosen one.
    ///
    /// Setting one clears the rest, because a drop-down shows one answer — which is
    /// the same rule a radio group follows and for the same reason.
    fn choose_option(&mut self, select: NodeId, option: NodeId) {
        for other in otlyra_dom::form::options_of(&self.document, select) {
            self.form.set_selectedness(other, other == option);
        }
        self.invalidate_styles();
    }

    /// Whether a list is showing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.interaction.open.is_some()
    }

    /// Put away whatever is open.
    pub fn close_open(&mut self) -> bool {
        self.set_interaction(otlyra_css::state::Interaction {
            open: None,
            ..self.interaction
        })
    }

    /// Move the chosen option of the focused `<select>` by one.
    ///
    /// What the arrows do on a drop-down, open or not: the specification leaves the
    /// interface to the browser, and every one of them moves the choice.
    pub fn step_selection(&mut self, forward: bool) -> bool {
        let Some(select) = self.interaction.focus else {
            return false;
        };
        if otlyra_dom::form::Control::of(&self.document, select)
            != Some(otlyra_dom::form::Control::Select)
        {
            return false;
        }
        let options: Vec<NodeId> = otlyra_dom::form::options_of(&self.document, select)
            .into_iter()
            .filter(|&option| !otlyra_dom::form::is_disabled(&self.document, option))
            .collect();
        if options.is_empty() {
            return false;
        }
        let current = options
            .iter()
            .position(|&option| self.form.selectedness(&self.document, option))
            .unwrap_or(0);
        let next = if forward {
            (current + 1).min(options.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        if next == current {
            return false;
        }
        self.choose_option(select, options[next]);
        true
    }

    /// The pointer came up. Runs the activation behaviour if it came up over what
    /// it went down on.
    pub fn pointer_released(&mut self, x: f64, y: f64) -> bool {
        self.field_dragging = false;
        let pressed = self.interaction.active;
        let over = self.control_at(x, y);
        let mut changed = self.set_interaction(otlyra_css::state::Interaction {
            active: None,
            ..self.interaction
        });
        if let Some(node) = pressed
            && over == pressed
        {
            changed |= self.activate(node);
        }
        changed
    }

    /// The form a press has asked to send, if it asked.
    ///
    /// Taken rather than read: a submission happens once, and leaving it here for a
    /// second frame to find would send the form twice.
    pub fn take_submission(&mut self) -> Option<otlyra_dom::Submission> {
        self.pending_submit.take()
    }

    /// Whether every control of `form` holds something acceptable.
    ///
    /// A form that does not is not sent, which is what stops a page from having to
    /// check anything itself. Every control the reader has been shown is marked as
    /// interacted with at the same time, so that `:user-invalid` lights up the
    /// fields that are wrong the moment the reader tries to send.
    fn validate(&mut self, form: NodeId) -> bool {
        let fields: Vec<NodeId> = descendants_of(&self.document, self.document.root())
            .into_iter()
            .filter(|&node| {
                otlyra_dom::form::form_owner(&self.document, node) == Some(form)
                    && otlyra_dom::form::is_validated(&self.document, node)
            })
            .collect();
        let mut ok = true;
        for field in fields {
            if otlyra_dom::form::validity(&self.document, &self.form, field).is_invalid() {
                ok = false;
                self.form.note_interaction(field);
            }
        }
        if !ok {
            self.invalidate_styles();
        }
        ok
    }

    /// Send the form `submitter` belongs to, unless it says not to check first.
    fn submit(&mut self, submitter: Option<NodeId>) -> bool {
        let Some(node) = submitter else {
            return false;
        };
        let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
            return false;
        };
        let skip_checking = attribute_of(&self.document, node, "formnovalidate").is_some()
            || attribute_of(&self.document, form, "novalidate").is_some();
        if !skip_checking && !self.validate(form) {
            return true;
        }
        self.pending_submit = Some(otlyra_dom::submit::submission(
            &self.document,
            &self.form,
            form,
            submitter,
        ));
        true
    }

    /// Put a form back the way the markup left it.
    fn reset(&mut self, form: NodeId) -> bool {
        self.form.reset(&self.document, Some(form));
        self.invalidate_styles();
        true
    }

    /// What activating a control does, with no script anywhere.
    ///
    /// HTML calls the checkbox and radio halves *legacy-pre-activation behaviour*
    /// and describes them exactly: a checkbox flips and stops being indeterminate;
    /// a radio button becomes the one that is checked, which unchecks every other
    /// member of its group. A group is the same tree, the same form owner and the
    /// same non-empty name.
    fn activate(&mut self, node: NodeId) -> bool {
        use otlyra_dom::form::{Control, InputKind};

        if !otlyra_dom::form::is_mutable(&self.document, node) {
            return false;
        }
        match Control::of(&self.document, node) {
            Some(Control::Input(InputKind::Checkbox)) => {
                let checked = self.form.checkedness(&self.document, node);
                self.form.set_checkedness(node, !checked);
            }
            Some(Control::Input(InputKind::Radio)) => {
                for member in otlyra_dom::form::radio_group(&self.document, node) {
                    self.form.set_checkedness(member, member == node);
                }
            }
            // A button sends the form it belongs to, or puts it back — which is the
            // whole of what a form does without a script, and what every form on
            // the web did before there was one.
            Some(Control::Input(InputKind::Submit | InputKind::Image) | Control::Button)
                if otlyra_dom::form::is_submit_button(&self.document, node) =>
            {
                self.form.note_interaction(node);
                return self.submit(Some(node));
            }
            Some(Control::Input(InputKind::Reset)) => {
                let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
                    return false;
                };
                return self.reset(form);
            }
            Some(Control::Button)
                if attribute_of(&self.document, node, "type")
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("reset")) =>
            {
                let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
                    return false;
                };
                return self.reset(form);
            }
            _ => return false,
        }
        self.form.note_interaction(node);
        self.invalidate_styles();
        true
    }

    /// Return in a field sends the form, if the form has a button to send it with
    /// or only one field to fill in.
    ///
    /// HTML calls it *implicit submission*, and it is why a search box with nothing
    /// but a field in it works at all.
    pub fn implicit_submit(&mut self) -> bool {
        let Some(node) = self.focused_field() else {
            return false;
        };
        let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
            return false;
        };
        let button = otlyra_dom::form::default_button(&self.document, Some(form));
        match button {
            Some(button) => self.submit(Some(button)),
            // No button to press: a form with exactly one field that takes typing
            // is still sent, and one with several is not.
            None => {
                let fields = descendants_of(&self.document, self.document.root())
                    .into_iter()
                    .filter(|&field| {
                        otlyra_dom::form::form_owner(&self.document, field) == Some(form)
                            && otlyra_dom::form::Control::of(&self.document, field)
                                .is_some_and(otlyra_dom::form::Control::is_text_entry)
                    })
                    .count();
                if fields == 1 {
                    self.submit_form(form)
                } else {
                    false
                }
            }
        }
    }

    /// Send a form with no button behind it.
    fn submit_form(&mut self, form: NodeId) -> bool {
        if attribute_of(&self.document, form, "novalidate").is_none() && !self.validate(form) {
            return true;
        }
        self.pending_submit = Some(otlyra_dom::submit::submission(
            &self.document,
            &self.form,
            form,
            None,
        ));
        true
    }

    /// The reader typed. Returns whether the page has to be drawn again.
    pub fn typed(&mut self, text: &str) -> bool {
        let Some(node) = self.focused_field() else {
            return false;
        };
        // Control characters are not text: a return, a tab and an escape arrive
        // here on some platforms and none of them is a letter.
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return false;
        }
        // What is typed over a selection replaces it.
        self.take_field_selection();
        let mut value = self.form.value(&self.document, node).to_owned();
        let at = self.caret.min(value.len());
        value.insert_str(at, &text);
        self.caret = at + text.len();
        self.field_anchor = Some(self.caret);
        self.restart_caret();
        self.form.set_value(node, value);
        self.invalidate_styles();
        true
    }

    /// The reader pressed a key that edits or moves the caret rather than typing.
    pub fn edit_text(&mut self, action: EditAction, extend: bool) -> bool {
        self.keyboard = true;
        self.restart_caret();
        let Some(node) = self.focused_field() else {
            return false;
        };
        // A backspace or a delete over a selection takes the selection, not one
        // character beside it.
        if matches!(action, EditAction::Backspace | EditAction::Delete)
            && self.take_field_selection()
        {
            self.invalidate_styles();
            return true;
        }
        let mut value = self.form.value(&self.document, node).to_owned();
        let at = self.caret.min(value.len());
        match action {
            EditAction::Backspace => {
                let Some(previous) = previous_boundary(&value, at) else {
                    return false;
                };
                value.replace_range(previous..at, "");
                self.caret = previous;
                self.field_anchor = Some(self.caret);
            }
            EditAction::Delete => {
                let Some(next) = next_boundary(&value, at) else {
                    return false;
                };
                value.replace_range(at..next, "");
                self.field_anchor = Some(self.caret);
            }
            // Moving the caret changes nothing the page holds, so it neither
            // restyles nor relays out — but it is still something that happened,
            // and once there is a caret to draw it is something to draw.
            //
            // Held with shift it takes the letters it passes; without, it drops
            // whatever was taken and goes to the end it was pushed towards — which
            // is why an arrow out of a selection lands at its edge rather than one
            // character in from where the caret happened to be.
            EditAction::Left | EditAction::Right | EditAction::Home | EditAction::End => {
                let collapsed = (!extend).then(|| self.field_selection()).flatten();
                self.caret = match (action, collapsed) {
                    (EditAction::Left, Some((_, from, _))) => from,
                    (EditAction::Right, Some((_, _, to))) => to,
                    (EditAction::Left, None) => previous_boundary(&value, at).unwrap_or(0),
                    (EditAction::Right, None) => next_boundary(&value, at).unwrap_or(value.len()),
                    (EditAction::Home, _) => 0,
                    (EditAction::End, _) => value.len(),
                    _ => at,
                };
                if !extend {
                    self.field_anchor = Some(self.caret);
                }
                self.show_ring();
                return true;
            }
        }
        self.form.set_value(node, value);
        self.invalidate_styles();
        true
    }

    /// The keyboard has been used, so the focus is to be shown from now on.
    fn show_ring(&mut self) -> bool {
        self.set_interaction(otlyra_css::state::Interaction {
            focus_visible: true,
            ..self.interaction
        })
    }

    /// What is selected inside the focused field, as a pair of offsets in order.
    ///
    /// Empty when the two ends are the same, which is a caret and not a selection.
    fn field_selection(&self) -> Option<(NodeId, usize, usize)> {
        let node = self.focused_field()?;
        let anchor = self.field_anchor?;
        let (from, to) = (anchor.min(self.caret), anchor.max(self.caret));
        (from < to).then_some((node, from, to))
    }

    /// Take out what is selected inside the focused field, leaving the caret where
    /// it was.
    ///
    /// What every editor does before it puts anything in: typing over a selection
    /// replaces it, and so does a backspace.
    fn take_field_selection(&mut self) -> bool {
        let Some((node, from, to)) = self.field_selection() else {
            return false;
        };
        let mut value = self.form.value(&self.document, node).to_owned();
        value.replace_range(from..to, "");
        self.caret = from;
        self.field_anchor = Some(from);
        self.form.set_value(node, value);
        true
    }

    /// Whether the page has a caret that has to keep being drawn.
    ///
    /// What makes the frame loop keep asking for frames: a caret that blinks is
    /// the one thing on a still page that changes on its own.
    #[must_use]
    pub fn caret_blinks(&self) -> bool {
        self.caret_source().is_some()
    }

    /// Whether the caret is showing this instant.
    ///
    /// Half a second on and half a second off, which is what every platform does
    /// and close enough to all of them that nobody will look twice. Measured from
    /// the last time the caret moved, so it is solid under the reader's fingers
    /// while they type and only starts blinking once they stop.
    fn caret_showing(&self) -> bool {
        const BLINK: u128 = 500;
        (self.caret_since.elapsed().as_millis() / BLINK).is_multiple_of(2)
    }

    /// The caret was put somewhere: start its blinking over.
    fn restart_caret(&mut self) {
        self.caret_since = std::time::Instant::now();
    }

    /// Pretend the caret was put where it is longer ago than it was.
    ///
    /// For the test of the blinking, which would otherwise have to sleep for half
    /// a second to watch half a blink.
    #[cfg(test)]
    fn wind_caret_back(&mut self, by: std::time::Duration) {
        self.caret_since -= by;
    }

    /// Slide the text inside a field so that the caret is inside it.
    ///
    /// A field is one line long however much is typed into it, so the line moves
    /// under the box. Which way it has to move is a question about where the caret
    /// is, and where the caret is takes a layout — so this lays the page out, looks,
    /// and lays it out again only when the answer moved. It moves when the caret
    /// reaches an edge and not on every letter.
    fn keep_caret_in_view(
        &mut self,
        caret: Option<Caret>,
        text: &mut TextEngine,
        width: f32,
        height: f32,
    ) {
        let Some(Caret::InField { box_id, offset }) = caret else {
            return;
        };
        let was = self.boxes.control_scroll(box_id);

        let fragments = self.fragments(text, width, height);
        let Some(caret_rect) = otlyra_layout::selection::caret_in(fragments, box_id, offset) else {
            return;
        };
        let Some(inner) = otlyra_layout::selection::content_box(fragments, box_id) else {
            return;
        };

        // A hair of room on each side, so a caret at the very end is beside the
        // edge rather than under it.
        const MARGIN: f32 = 1.0;
        let mut now = was;
        if caret_rect.right() > inner.right() - MARGIN {
            now.0 += caret_rect.right() - (inner.right() - MARGIN);
        }
        if caret_rect.x < inner.x + MARGIN {
            now.0 -= (inner.x + MARGIN) - caret_rect.x;
        }
        if caret_rect.bottom() > inner.bottom() {
            now.1 += caret_rect.bottom() - inner.bottom();
        }
        if caret_rect.y < inner.y {
            now.1 -= inner.y - caret_rect.y;
        }
        // Never past the start: a field with room to spare shows its first letter
        // at its left edge and its first line at its top, not a gap where one used
        // to be.
        now = (now.0.max(0.0), now.1.max(0.0));

        if (now.0 - was.0).abs() > 0.01 || (now.1 - was.1).abs() > 0.01 {
            self.boxes.set_control_scroll(box_id, now);
            self.layout_stale = true;
        }
    }

    /// The box an open drop-down's list is in, if one is open.
    fn open_list(&self) -> Option<BoxId> {
        let select = self.interaction.open?;
        let box_id = self.boxes.box_for(select)?;
        self.boxes
            .node(box_id)
            .children
            .iter()
            .copied()
            .find(|&child| self.boxes.node(child).control.is_some())
    }

    /// Slide an open list so that the chosen option is in it.
    ///
    /// The same shape as keeping the caret in sight, and for the same reason: how
    /// far it has to move is only known once it has been laid out, so the page lays
    /// out, looks, and lays out again when the answer moved.
    fn keep_choice_in_view(&mut self, text: &mut TextEngine, width: f32, height: f32) {
        let Some(list) = self.open_list() else {
            return;
        };
        let Some(select) = self.interaction.open else {
            return;
        };
        let chosen = otlyra_dom::form::options_of(&self.document, select)
            .into_iter()
            .find(|&option| self.form.selectedness(&self.document, option))
            .and_then(|option| self.boxes.box_for(option));
        let Some(chosen) = chosen else { return };

        let was = self.boxes.control_scroll(list);
        let fragments = self.fragments(text, width, height);
        let Some(option) = otlyra_layout::selection::content_box(fragments, chosen) else {
            return;
        };
        let Some(inner) = otlyra_layout::selection::content_box(fragments, list) else {
            return;
        };

        let mut now = was;
        if option.bottom() > inner.bottom() {
            now.1 += option.bottom() - inner.bottom();
        }
        if option.y < inner.y {
            now.1 -= inner.y - option.y;
        }
        now.1 = now.1.max(0.0);
        if (now.1 - was.1).abs() > 0.01 {
            self.boxes.set_control_scroll(list, now);
            self.layout_stale = true;
        }
    }

    /// What the caret belongs to, if the page has one.
    ///
    /// One caret and one answer, whether it is in a field or in the page's own
    /// text: a field's is a byte offset into what the control holds and the page's
    /// is a collapsed selection, and both end up going through the same arithmetic
    /// the selection's own edges go through. Two answers is how a caret ends up a
    /// pixel away from the letter it is supposed to be beside.
    fn caret_source(&self) -> Option<Caret> {
        if let Some(node) = self.focused_field() {
            let box_id = self.boxes.box_for(node)?;
            let value = self.form.value(&self.document, node);
            return Some(Caret::InField {
                box_id,
                offset: self.caret.min(value.len()),
            });
        }
        // A collapsed selection is a caret: it is where the last click landed and
        // where a shift and an arrow would start extending from.
        let selection = self.selection?;
        selection
            .is_empty()
            .then_some(Caret::InPage(selection.focus))
    }

    /// What the focused control holds, for a panel that wants to show it.
    pub fn focused_value(&self) -> Option<&str> {
        let node = self.interaction.focus?;
        Some(self.form.value(&self.document, node))
    }

    /// The `href` of the link at `point`, if there is one.
    ///
    /// Walks up the box tree, because the text inside `<a><b>text</b></a>` belongs
    /// to the `<b>` and the link is two boxes above it.
    pub fn link_at(&self, x: f64, y: f64) -> Option<String> {
        let mut current = self.box_at(x, y);
        while let Some(id) = current {
            let node = self.boxes.get(id)?;
            if node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "a")
                && let Some(href) = node.node.and_then(|node| self.attribute(node, "href"))
            {
                return Some(href);
            }
            current = node.parent;
        }
        None
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

    /// Where a box was drawn on the last frame, if it was.
    pub fn rect_of(&self, id: BoxId) -> Option<otlyra_layout::Rect> {
        self.targets
            .iter()
            .find(|(_, target)| *target == id)
            .map(|(rect, _)| {
                otlyra_layout::Rect::new(
                    rect.x0 as f32,
                    rect.y0 as f32,
                    rect.width() as f32,
                    rect.height() as f32,
                )
            })
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

    /// The `href` of a box, if it is a link with one.
    pub fn href_of(&self, id: BoxId) -> Option<String> {
        let node = self.boxes.get(id)?;
        if node.tag.as_ref().is_none_or(|tag| tag.as_ref() != "a") {
            return None;
        }
        self.attribute(node.node?, "href")
    }

    /// One attribute of an element node.
    fn attribute(&self, node: NodeId, name: &str) -> Option<String> {
        self.document
            .get(node)?
            .element()?
            .attrs
            .iter()
            .find(|attr| attr.name.local.as_ref() == name)
            .map(|attr| attr.value.to_string())
    }

    /// Put the reader back where they were, as a reload does.
    ///
    /// Not clamped here: the new document may be shorter or taller, and the clamp
    /// happens on the next scroll or the next frame, once there is a layout to
    /// clamp against.
    pub fn set_scroll(&mut self, scroll: f32) {
        self.scroll = scroll.max(0.0);
        self.damage.add(Damage::PAINT);
    }

    /// Draw no scrollbars, for a picture that is going to be compared with one
    /// from elsewhere.
    pub fn hide_scrollbars(&mut self) {
        self.scrollbars = false;
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

    /// Begin a selection at a point in the window, and answer whether there is
    /// any text there to select.
    ///
    /// The page coordinates are the window's plus wherever the reader has scrolled
    /// to, which is the one conversion between what a pointer reports and what a
    /// page is laid out in.
    pub fn select_from(&mut self, x: f32, y: f32, top: f32) -> bool {
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        let point = (x, y - top + self.scroll);
        match otlyra_layout::selection::position_at(tree, point.0, point.1) {
            Some(position) => {
                self.set_selection(Some(otlyra_layout::Selection::at(position)));
                true
            }
            None => {
                self.set_selection(None);
                false
            }
        }
    }

    /// Take the selection to a point: what a drag does after the press.
    pub fn select_to(&mut self, x: f32, y: f32, top: f32) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return;
        };
        let point = (x, y - top + self.scroll);
        if let Some(position) = otlyra_layout::selection::position_at(tree, point.0, point.1) {
            selection.focus = position;
            self.set_selection(Some(selection));
        }
    }

    /// Take the word under a point, which is what a second click means.
    pub fn select_word_at(&mut self, x: f32, y: f32, top: f32) -> bool {
        self.select_expanded(x, y, top, otlyra_layout::selection::word_at)
    }

    /// And the block it is in, which is what a third means.
    pub fn select_paragraph_at(&mut self, x: f32, y: f32, top: f32) -> bool {
        self.select_expanded(x, y, top, otlyra_layout::selection::paragraph_at)
    }

    fn select_expanded(
        &mut self,
        x: f32,
        y: f32,
        top: f32,
        expand: fn(&FragmentTree, otlyra_layout::TextPosition) -> otlyra_layout::Selection,
    ) -> bool {
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        let point = (x, y - top + self.scroll);
        match otlyra_layout::selection::position_at(tree, point.0, point.1) {
            Some(position) => {
                let selection = expand(tree, position);
                self.set_selection(Some(selection));
                !selection.is_empty()
            }
            None => {
                self.set_selection(None);
                false
            }
        }
    }

    /// Select the whole page.
    pub fn select_all(&mut self) -> bool {
        // A field with the focus is what "everything" means: selecting the page
        // behind it is not what a reader who is typing into one asked for.
        if let Some(node) = self.focused_field() {
            let value_len = self.form.value(&self.document, node).len();
            self.field_anchor = Some(0);
            self.caret = value_len;
            self.restart_caret();
            return true;
        }
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        match otlyra_layout::selection::all(tree) {
            Some(selection) if !selection.is_empty() => {
                self.set_selection(Some(selection));
                true
            }
            _ => false,
        }
    }

    /// Move the far end of the selection one step, and answer whether anything
    /// moved.
    ///
    /// Extending keeps where it started and moves where it is going; not
    /// extending takes both ends to the same place, which is a caret rather than
    /// a selection and is what an arrow key means with nothing held down.
    pub fn move_selection(&mut self, motion: otlyra_layout::Motion, extend: bool) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        let focus = otlyra_layout::selection::moved(tree, selection.focus, motion);
        let moved = otlyra_layout::Selection {
            anchor: if extend { selection.anchor } else { focus },
            focus,
        };
        if moved == selection {
            return false;
        }
        self.set_selection(Some(moved));
        true
    }

    /// Drop the selection, which is what a press somewhere else means.
    pub fn clear_selection(&mut self) {
        self.set_selection(None);
    }

    /// What is selected, as text, or `None` when nothing is.
    pub fn selected_text(&self) -> Option<String> {
        if let Some((node, from, to)) = self.field_selection() {
            return Some(self.form.value(&self.document, node)[from..to].to_owned());
        }
        let selection = self.selection.filter(|selection| !selection.is_empty())?;
        let (_, tree) = self.layout.as_ref()?;
        let text = otlyra_layout::selection::text(tree, selection);
        (!text.is_empty()).then_some(text)
    }

    /// Whether anything is selected.
    pub fn has_selection(&self) -> bool {
        if self.field_selection().is_some() {
            return true;
        }
        self.selection
            .is_some_and(|selection| !selection.is_empty())
    }

    fn set_selection(&mut self, selection: Option<otlyra_layout::Selection>) {
        if self.selection == selection {
            return;
        }
        self.selection = selection;
        self.damage.add(otlyra_layout::Damage::PAINT);
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

    /// Take hold of a scrollbar under (`x`, `y`), if one is there.
    ///
    /// Returns whether it grabbed anything: a press that lands on a scrollbar
    /// belongs to it and not to the page behind it.
    pub fn grab_scrollbar(&mut self, x: f32, y: f32, width: f32, height: f32) -> bool {
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };

        // The page's own bar first: it is drawn over everything, so it is grabbed
        // before anything under it.
        let page_area = otlyra_layout::fragment::Rect::new(0.0, 0.0, width, height);
        if let Some(thumb) =
            otlyra_paint::scrollbar_thumb(page_area, tree.content_height(), self.scroll)
            && contains(thumb, x, y)
        {
            self.drag = Some(Drag {
                target: None,
                grabbed_at: y - thumb.y,
            });
            return true;
        }

        for port in &tree.scroll_ports {
            let mut area = port.port;
            area.y -= self.scroll;
            let at = self.port_scroll.get(&port.id).copied().unwrap_or(0.0);
            if let Some(thumb) = otlyra_paint::scrollbar_thumb(area, port.content_height, at)
                && contains(thumb, x, y)
            {
                self.drag = Some(Drag {
                    target: Some(port.id),
                    grabbed_at: y - thumb.y,
                });
                return true;
            }
        }

        false
    }

    /// Whether a scrollbar is being dragged.
    pub fn dragging_scrollbar(&self) -> bool {
        self.drag.is_some()
    }

    /// Let go of whatever was grabbed.
    pub fn release_scrollbar(&mut self) {
        self.drag = None;
    }

    /// Drag the grabbed scrollbar to `y`.
    ///
    /// The thumb follows the pointer and the content follows the thumb, which is
    /// the way round that makes a drag feel attached to the hand rather than to the
    /// document.
    pub fn drag_scrollbar(&mut self, y: f32, width: f32, height: f32) {
        let Some(drag) = self.drag else {
            return;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return;
        };

        let (area, content, range) = match drag.target {
            None => {
                let area = otlyra_layout::fragment::Rect::new(0.0, 0.0, width, height);
                let content = tree.content_height();
                (area, content, (content - height).max(0.0))
            }
            Some(id) => {
                let Some(port) = tree.scroll_ports.iter().find(|port| port.id == id) else {
                    return;
                };
                let mut area = port.port;
                area.y -= self.scroll;
                (area, port.content_height, port.range())
            }
        };

        let travel = otlyra_paint::scrollbar_travel(area, content);
        if travel <= 0.0 {
            return;
        }
        let wanted = ((y - drag.grabbed_at - area.y) / travel).clamp(0.0, 1.0) * range;

        match drag.target {
            None => self.set_scroll(wanted),
            Some(id) => {
                self.port_scroll.insert(id, wanted);
                self.damage.add(Damage::PAINT);
            }
        }
    }

    /// Scroll whatever is under (`x`, `y`) by `delta` logical pixels.
    ///
    /// A box that cuts its contents off and has more of them than it can show takes
    /// the wheel before the page does, and hands it back once it has reached its
    /// end — which is what makes a scrollable panel inside a page feel right rather
    /// than trapping the reader in it.
    pub fn scroll_at(&mut self, x: f32, y: f32, delta: f32) {
        let page_point = (x, y + self.scroll);
        let port = self.layout.as_ref().and_then(|(_, tree)| {
            // Innermost last: a port inside a port is pushed after it.
            tree.scroll_ports
                .iter()
                .rev()
                .find(|port| {
                    let offset = self.port_scroll.get(&port.id).copied().unwrap_or(0.0);
                    let _ = offset;
                    let rect = port.port;
                    page_point.0 >= rect.x
                        && page_point.0 < rect.right()
                        && page_point.1 >= rect.y
                        && page_point.1 < rect.bottom()
                })
                .copied()
        });

        if let Some(port) = port {
            let at = self.port_scroll.entry(port.id).or_insert(0.0);
            let wanted = *at + delta;
            let clamped = wanted.clamp(0.0, port.range());
            if (clamped - *at).abs() > f32::EPSILON {
                *at = clamped;
                self.damage.add(Damage::PAINT);
                return;
            }
            // At its end: the page takes the rest, rather than the wheel doing
            // nothing at all.
        }

        self.scroll_by(delta);
    }

    /// Scroll the page by `delta` logical pixels, clamped to the content.
    ///
    /// Damages paint and no more: where the content is has not changed, only which
    /// part of it is on screen.
    pub fn scroll_by(&mut self, delta: f32) {
        self.damage.add(Damage::PAINT);
        let content = self
            .layout
            .as_ref()
            .map_or(0.0, |(_, tree)| tree.content_height());
        let max = (content - self.viewport_height).max(0.0);
        self.scroll = (self.scroll + delta).clamp(0.0, max);
    }
}

/// A scrollbar being dragged.
#[derive(Copy, Clone, Debug)]
struct Drag {
    /// Which scroll port's bar, or the page's own.
    target: Option<BoxId>,
    /// Where on the thumb it was taken hold of, so it does not jump to the pointer.
    grabbed_at: f32,
}

/// Whether a rectangle contains a point.
fn contains(rect: otlyra_layout::fragment::Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
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
mod tests {
    use otlyra_gfx::{DisplayItem, PaintOp, RecordingPainter, render};

    use super::*;

    fn scene(html: &str) -> (PageScene, TextEngine) {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        (PageScene::new(parsed.document), TextEngine::isolated())
    }

    /// The seam the inspector was reading-only for want of: an edit goes in and
    /// the next frame is the page as edited, restyled, relaid and repainted.
    #[test]
    fn an_edit_restyles_the_page_and_the_next_frame_shows_it() {
        let (mut page, mut text) =
            scene("<style>p { color: red } .big { font-size: 40px }</style><body><p>text");
        let before = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let builds = page.builds();

        let paragraph = {
            let document = page.document();
            let mut stack = vec![document.root()];
            let mut found = None;
            while let Some(node) = stack.pop() {
                stack.extend(document.children(node));
                if matches!(document.get(node).map(|n| &n.data),
                    Some(NodeData::Element(element)) if element.name.local.as_ref() == "p")
                {
                    found = Some(node);
                }
            }
            found.expect("the document has a p")
        };

        // A class that a rule in the page is waiting for.
        assert!(page.edit(|document| document.set_attr(paragraph, "class", "big")));
        let after = page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        assert!(page.builds() > builds, "an edit is a frame to build again");
        assert_ne!(before, after, "and the page is drawn differently for it");

        // The cascade really ran: the rule the class selects took effect.
        let glyph_height = |list: &DisplayList| {
            list.items()
                .iter()
                .filter_map(|item| match item {
                    DisplayItem::Glyphs { font_size, .. } => Some(*font_size),
                    _ => None,
                })
                .fold(0.0_f32, f32::max)
        };
        assert!(
            glyph_height(&after) > glyph_height(&before),
            "the class brought a bigger font size with it"
        );

        // And it settles again: an edited page with a still reader is as idle as
        // any other.
        let builds = page.builds();
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), builds);
    }

    /// W10's page half: an idle page with a still reader does no work. Every
    /// mutation records damage and the frame reads it, which is what `Damage`
    /// was written for and what nothing did until now.
    #[test]
    fn an_unchanged_page_is_not_painted_a_second_time() {
        let (mut page, mut text) = scene("<body><h1>Title</h1><p>Some text to lay out.</p>");
        let first = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 1);

        let again = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 1, "nothing about it moved");
        assert_eq!(first, again, "and the frame is the same frame");

        // Scrolling is a repaint, and a repaint is what it asks for.
        page.set_scroll(40.0);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 2);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 2, "and then it is still again");

        // A resize is a relayout, and a different band of the window to draw in
        // is a different frame even when nothing else moved.
        page.build_display_list(&mut text, 700.0, 600.0, 0.0);
        assert_eq!(page.builds(), 3);
        page.build_display_list(&mut text, 700.0, 600.0, 12.0);
        assert_eq!(page.builds(), 4, "the page moved down the window");
    }

    /// A press still lands on what is on screen when the frame was reused: the
    /// targets came out of that very list, so they describe it exactly.
    #[test]
    fn a_reused_frame_is_still_the_frame_a_press_is_tested_against() {
        let (mut page, mut text) = scene("<body><p><a href=\"/next\">go</a></p>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let before = page.boxes().root();
        let _ = before;
        let hit = (0..600)
            .step_by(4)
            .find_map(|y| page.box_at(20.0, f64::from(y)));

        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 1, "the frame was reused");
        assert_eq!(
            hit,
            (0..600)
                .step_by(4)
                .find_map(|y| page.box_at(20.0, f64::from(y))),
            "and it still answers where things are"
        );
    }

    /// Where the first box of `tag` is, in window coordinates.
    fn point_on(page: &PageScene, tag: &str) -> (f64, f64) {
        for y in (0..600).step_by(2) {
            for x in (0..800).step_by(2) {
                if let Some(id) = page.box_at(f64::from(x), f64::from(y))
                    && page
                        .boxes()
                        .get(id)
                        .and_then(|node| node.tag.clone())
                        .is_some_and(|found| found.as_ref() == tag)
                {
                    return (f64::from(x), f64::from(y));
                }
            }
        }
        panic!("no {tag} on the page");
    }

    /// The text every box in the page draws, run together.
    fn page_text(page: &PageScene) -> String {
        let tree = page.boxes();
        tree.descendants(tree.root())
            .into_iter()
            .filter_map(|id| match &tree.node(id).kind {
                otlyra_layout::box_tree::BoxKind::Text(text) => Some(text.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Whether the box the given tag generated is drawn as checked.
    fn is_checked(page: &PageScene, tag: &str) -> bool {
        let tree = page.boxes();
        tree.descendants(tree.root()).into_iter().any(|id| {
            let node = tree.node(id);
            node.tag.as_ref().is_some_and(|found| found.as_ref() == tag)
                && node
                    .control
                    .as_ref()
                    .is_some_and(|control| control.state.checked)
        })
    }

    #[test]
    fn a_press_and_a_release_on_a_checkbox_tick_it() {
        let (mut page, mut text) = scene("<body><input type=checkbox>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");

        assert!(!is_checked(&page, "input"));
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(is_checked(&page, "input"), "a click ticks it");

        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(!is_checked(&page, "input"), "and the next one unticks it");
    }

    /// A press that wanders off before it is let go takes itself back, which is
    /// what a press means on every platform.
    #[test]
    fn a_release_somewhere_else_does_not_tick_it() {
        let (mut page, mut text) = scene("<body><input type=checkbox>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");

        page.pointer_pressed(x, y);
        page.pointer_released(x + 400.0, y + 200.0);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(!is_checked(&page, "input"));
    }

    /// Clicking the words beside a checkbox ticks it, because a label's activation
    /// behaviour is its control's.
    #[test]
    fn a_label_passes_the_press_to_what_it_names() {
        let (mut page, mut text) =
            scene("<body><label for=agree>I agree</label><input id=agree type=checkbox>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "label");

        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(is_checked(&page, "input"));
    }

    #[test]
    fn only_one_radio_button_in_a_group_is_checked_at_a_time() {
        let (mut page, mut text) =
            scene("<body><input type=radio name=g value=a><input type=radio name=g value=b>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        let checked_values = |page: &PageScene| -> Vec<String> {
            let tree = page.boxes();
            tree.descendants(tree.root())
                .into_iter()
                .filter(|&id| {
                    tree.node(id)
                        .control
                        .as_ref()
                        .is_some_and(|control| control.state.checked)
                })
                .filter_map(|id| {
                    let node = tree.node(id).node?;
                    page.document()
                        .get(node)?
                        .element()?
                        .attr("value")
                        .map(str::to_owned)
                })
                .collect()
        };

        // The two are side by side; the first is at the left edge and the second
        // beyond it.
        let mut seen = Vec::new();
        for x in (0..200).step_by(2) {
            if let Some(id) = page.box_at(f64::from(x), 12.0)
                && page
                    .boxes()
                    .get(id)
                    .and_then(|node| node.control.clone())
                    .is_some()
                && !seen.contains(&id)
            {
                seen.push(id);
            }
        }
        assert!(seen.len() >= 2, "both radio buttons are on the page");

        let first = page.rect_of(seen[0]).expect("a rectangle");
        let second = page.rect_of(seen[1]).expect("a rectangle");
        let click = |page: &mut PageScene, rect: otlyra_layout::Rect| {
            let (x, y) = (
                f64::from(rect.x + rect.width / 2.0),
                f64::from(rect.y + rect.height / 2.0),
            );
            page.pointer_pressed(x, y);
            page.pointer_released(x, y);
        };

        click(&mut page, first);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(checked_values(&page), ["a"]);

        click(&mut page, second);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(checked_values(&page), ["b"], "the first one gave way");
    }

    #[test]
    fn typing_into_a_field_shows_what_was_typed() {
        let (mut page, mut text) = scene("<body><input>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");

        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        assert!(page.typed("Ada"));
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page_text(&page), "Ada");

        assert!(page.edit_text(EditAction::Backspace, false));
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page_text(&page), "Ad");

        // The caret is where the typing goes, and it moves.
        assert!(page.edit_text(EditAction::Home, false));
        assert!(page.typed("M"));
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page_text(&page), "MAd");
    }

    /// The caret a page draws, if it draws one.
    fn caret_of(page: &mut PageScene, text: &mut TextEngine) -> Option<(f64, f64)> {
        let list = page.build_display_list(text, 800.0, 600.0, 0.0);
        // The caret is the last thing drawn, and it is one pixel wide.
        list.items().iter().rev().find_map(|item| match item {
            DisplayItem::Fill { shape, .. } => {
                let bounds = otlyra_gfx::kurbo::Shape::bounding_box(shape);
                (bounds.width() - 1.0)
                    .abs()
                    .lt(&0.01)
                    .then_some((bounds.x0, bounds.y0))
            }
            _ => None,
        })
    }

    #[test]
    fn a_field_with_the_focus_shows_a_caret_that_moves_with_the_typing() {
        let (mut page, mut text) = scene("<body><input>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(
            caret_of(&mut page, &mut text).is_none(),
            "a page nobody has clicked has no caret"
        );

        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        let empty = caret_of(&mut page, &mut text).expect("an empty field still has a caret");

        page.typed("Ada");
        let typed = caret_of(&mut page, &mut text).expect("and a full one has one too");
        assert!(
            typed.0 > empty.0,
            "the caret is past what was typed: {typed:?} against {empty:?}"
        );

        page.edit_text(EditAction::Home, false);
        let home = caret_of(&mut page, &mut text).expect("a caret at the start");
        assert!(
            (home.0 - empty.0).abs() < 0.5,
            "and back where it started: {home:?} against {empty:?}"
        );
    }

    #[test]
    fn a_click_into_a_field_puts_the_caret_where_it_landed() {
        let (mut page, mut text) = scene("<body><input value=abcdefghij>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");

        // Near the left edge of the text: before what is there rather than after.
        page.pointer_pressed(x + 3.0, y);
        page.pointer_released(x + 3.0, y);
        page.typed("Z");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let started = page_text(&page);
        assert!(
            started.starts_with('Z'),
            "the caret landed at the start: {started:?}"
        );

        // Well past the end of the text: after all of it.
        page.pointer_pressed(x + 400.0, y);
        page.pointer_released(x + 400.0, y);
        assert!(!page.typed("Q"), "and that is not in the field at all");
    }

    /// A drag inside a field takes the letters it passes, and what is typed over a
    /// selection replaces it.
    #[test]
    fn a_field_has_a_selection_of_its_own() {
        let (mut page, mut text) = scene("<body><input value=abcdefgh>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");

        // From the very start to well past the end: everything.
        page.pointer_pressed(x, y);
        // A frame between the two, as there is in the window: the press restyles,
        // and a drag is tested against the layout the last frame was drawn from.
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        page.pointer_moved(x + 400.0, y);
        page.pointer_released(x + 400.0, y);
        assert_eq!(page.selected_text().as_deref(), Some("abcdefgh"));
        assert!(page.has_selection());

        // Typed over, it goes.
        page.typed("Z");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page_text(&page), "Z");
        assert!(!page.has_selection(), "and nothing is selected afterwards");
    }

    #[test]
    fn shift_and_an_arrow_take_the_letters_they_pass_and_a_bare_one_does_not() {
        let (mut page, mut text) = scene("<body><input value=abcd>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.edit_text(EditAction::End, false);

        page.edit_text(EditAction::Left, true);
        page.edit_text(EditAction::Left, true);
        assert_eq!(page.selected_text().as_deref(), Some("cd"));

        // Without shift it drops what was taken and lands at the edge it was
        // pushed towards rather than one further in.
        page.edit_text(EditAction::Left, false);
        assert!(!page.has_selection());
        page.typed("-");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page_text(&page), "ab-cd");
    }

    #[test]
    fn selecting_everything_in_a_focused_field_takes_the_field_and_not_the_page() {
        let (mut page, mut text) = scene("<body><p>prose</p><input value=typed>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);

        assert!(page.select_all());
        assert_eq!(page.selected_text().as_deref(), Some("typed"));

        // A backspace over it takes the whole of it.
        page.edit_text(EditAction::Backspace, false);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(!page_text(&page).contains("typed"));
    }

    /// An open drop-down shows its options over the page rather than in it, so
    /// opening one moves nothing behind it.
    #[test]
    fn a_drop_down_opens_over_the_page_and_choosing_puts_it_away() {
        let (mut page, mut text) = scene(
            "<body><p id=before>before</p>\
             <select><option>Alpha</option><option>Beta</option></select>\
             <p id=after>after</p>",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "select");

        let after_y = |page: &PageScene| {
            let tree = page.boxes();
            tree.descendants(tree.root())
                .into_iter()
                .find(|&id| {
                    tree.node(id).node.and_then(|node| {
                        page.document()
                            .get(node)
                            .and_then(|inner| inner.element())
                            .and_then(|element| element.id())
                            .map(str::to_owned)
                    }) == Some("after".to_owned())
                })
                .and_then(|id| page.rect_of(id))
                .map(|rect| rect.y)
        };

        let closed = after_y(&page);
        assert!(closed.is_some());
        assert!(!page.is_open());
        assert!(
            !page_text(&page).contains("Beta"),
            "a closed one shows one option"
        );

        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(page.is_open());
        assert!(
            page_text(&page).contains("Beta"),
            "an open one shows the list"
        );
        assert_eq!(
            after_y(&page),
            closed,
            "and the page behind it did not move"
        );

        // Pressing the control again puts the list away.
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(!page.is_open());
    }

    /// Laying the same box tree out twice must give the same box: the room a
    /// drop-down leaves for its arrow is added to its padding, and adding it again
    /// on every pass made the control grow twenty pixels a frame.
    #[test]
    fn laying_a_drop_down_out_twice_does_not_widen_it() {
        let (mut page, mut text) =
            scene("<body><select><option>Alpha</option><option>Beta</option></select>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let select = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .find(|&id| page.boxes().node(id).control.is_some())
            .expect("the select");
        let first = page.rect_of(select).expect("a rectangle");

        // A resize lays the same tree out again, and again.
        for width in [799.0, 798.0, 797.0, 796.0, 800.0] {
            page.build_display_list(&mut text, width, 600.0, 0.0);
        }
        assert_eq!(page.rect_of(select), Some(first));
    }

    /// A form that submits is a form that navigates, and pressing the button is
    /// the whole of it.
    #[test]
    fn pressing_a_submit_button_sends_the_form() {
        let (mut page, mut text) = scene(
            "<body><form action=/search><input name=q value=\"a b\">\
             <input type=submit value=Go></form>",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(page.take_submission().is_none(), "nothing has been pressed");

        // The button is the second control on the line.
        let button = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .filter(|&id| page.boxes().node(id).control.is_some())
            .nth(1)
            .expect("the button");
        let rect = page.rect_of(button).expect("a rectangle");
        let (x, y) = (
            f64::from(rect.x + rect.width / 2.0),
            f64::from(rect.y + rect.height / 2.0),
        );
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);

        let sent = page.take_submission().expect("the form was sent");
        assert_eq!(sent.url, "/search?q=a+b");
        assert!(page.take_submission().is_none(), "and only once");
    }

    /// A form with something wrong in it is not sent, and the field that is wrong
    /// is marked so that a rule can show it.
    #[test]
    fn a_form_that_does_not_check_out_is_not_sent() {
        let (mut page, mut text) = scene(
            "<body><form action=/save><input name=who required>\
             <input type=submit value=Go></form>",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let button = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .filter(|&id| page.boxes().node(id).control.is_some())
            .nth(1)
            .expect("the button");
        let rect = page.rect_of(button).expect("a rectangle");
        let (x, y) = (
            f64::from(rect.x + rect.width / 2.0),
            f64::from(rect.y + rect.height / 2.0),
        );
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        assert!(page.take_submission().is_none(), "an empty required field");

        // Fill it in, and it goes.
        let field = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .find(|&id| page.boxes().node(id).control.is_some())
            .expect("the field");
        let rect = page.rect_of(field).expect("a rectangle");
        page.pointer_pressed(f64::from(rect.x + 4.0), f64::from(rect.y + 4.0));
        page.pointer_released(f64::from(rect.x + 4.0), f64::from(rect.y + 4.0));
        page.typed("Ada");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // The box tree was built again, so the handles from before are stale.
        let button = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .filter(|&id| page.boxes().node(id).control.is_some())
            .nth(1)
            .expect("the button");
        let rect = page.rect_of(button).expect("a rectangle");
        let (x, y) = (
            f64::from(rect.x + rect.width / 2.0),
            f64::from(rect.y + rect.height / 2.0),
        );
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        assert_eq!(
            page.take_submission().map(|sent| sent.url),
            Some("/save?who=Ada".to_owned())
        );
    }

    /// Return in a field sends the form, which is why a search box with nothing but
    /// a field in it works at all.
    #[test]
    fn return_in_the_only_field_sends_the_form() {
        let (mut page, mut text) =
            scene("<body><form action=/search><input name=q value=cats></form>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        assert!(page.implicit_submit());
        assert_eq!(
            page.take_submission().map(|sent| sent.url),
            Some("/search?q=cats".to_owned())
        );
    }

    /// A reset button puts a form back the way the markup left it.
    #[test]
    fn a_reset_button_puts_the_form_back() {
        let (mut page, mut text) =
            scene("<body><form><input value=start><input type=reset value=Reset></form>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.typed("!");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(page_text(&page).contains('!'));

        let reset = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .filter(|&id| page.boxes().node(id).control.is_some())
            .nth(1)
            .expect("the reset button");
        let rect = page.rect_of(reset).expect("a rectangle");
        let (x, y) = (
            f64::from(rect.x + rect.width / 2.0),
            f64::from(rect.y + rect.height / 2.0),
        );
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(
            !page_text(&page).contains('!'),
            "back to what the markup said"
        );
    }

    /// A list of two hundred countries is not a list two hundred rows long: it is
    /// capped, and it slides so that what is chosen is in it.
    #[test]
    fn a_long_open_list_is_capped_and_slides_to_the_choice() {
        let options: String = (0..60)
            .map(|n| format!("<option>Row {n}</option>"))
            .collect();
        let (mut page, mut text) = scene(&format!("<body><select>{options}</select>"));
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "select");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        let list = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .find(|&id| page.boxes().node(id).anonymous && page.boxes().node(id).control.is_some())
            .expect("the open list");
        let rect = page.rect_of(list).expect("a rectangle");
        // The cap is on the content box; the border it has is its own.
        assert!(rect.height <= 310.0, "capped, got {}", rect.height);

        // Walk to the far end: the list has to have moved to show it.
        for _ in 0..40 {
            page.step_selection(true);
        }
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(
            page.boxes().control_scroll(list).1 > 0.0,
            "the list slid to what is chosen"
        );
    }

    #[test]
    fn the_arrows_move_a_drop_downs_choice() {
        let (mut page, mut text) =
            scene("<body><select><option>Alpha</option><option>Beta</option></select>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "select");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.close_open();
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(page_text(&page).contains("Alpha"));

        assert!(page.step_selection(true));
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(
            page_text(&page).contains("Beta"),
            "and it shows the new one"
        );

        assert!(!page.step_selection(true), "the last one is the last one");
        assert!(page.step_selection(false));
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(page_text(&page).contains("Alpha"));
    }

    #[test]
    fn a_second_press_in_a_field_takes_the_word_and_a_third_takes_all_of_it() {
        let (mut page, mut text) = scene("<body><input value=\"one two three\" size=40>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");

        // Somewhere inside the middle word.
        let middle = x + 30.0;
        page.pointer_pressed_times(middle, y, 2);
        page.pointer_released(middle, y);
        assert_eq!(page.selected_text().as_deref(), Some("two"));

        page.pointer_pressed_times(middle, y, 3);
        page.pointer_released(middle, y);
        assert_eq!(page.selected_text().as_deref(), Some("one two three"));
    }

    /// A text area is as many rows as it was asked for however much is in it: the
    /// text slides up under the box so the caret stays where it can be seen.
    #[test]
    fn a_text_area_slides_its_lines_to_keep_the_caret_in_sight() {
        let (mut page, mut text) = scene("<body><textarea rows=2 cols=10></textarea>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "textarea");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);

        let area = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .find(|&id| page.boxes().node(id).control.is_some())
            .expect("the text area");

        page.typed("one");
        caret_of(&mut page, &mut text);
        assert_eq!(page.boxes().control_scroll(area).1, 0.0, "two rows fit two");

        // Far more lines than it shows.
        for _ in 0..8 {
            page.typed(" wrapping words that go on");
        }
        let after = caret_of(&mut page, &mut text).expect("a caret still");
        assert!(
            page.boxes().control_scroll(area).1 > 0.0,
            "the lines slid up under the box"
        );
        let rect = page.rect_of(area).expect("a rectangle");
        assert!(
            after.1 <= f64::from(rect.bottom()),
            "and the caret is inside it: {after:?} against {rect:?}"
        );
    }

    /// A caret is solid while the reader types and blinks once they stop, which is
    /// what every platform does and what keeps it visible exactly when it is being
    /// looked for.
    #[test]
    fn the_caret_blinks_and_starts_over_on_every_keystroke() {
        let (mut page, mut text) = scene("<body><input>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(
            !page.caret_blinks(),
            "a page nobody has clicked has no caret"
        );

        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        assert!(
            page.caret_blinks(),
            "and one that has been clicked into has"
        );
        assert!(
            caret_of(&mut page, &mut text).is_some(),
            "solid the instant it is put there"
        );

        // Half a second on, half a second off. Rather than sleeping for one, the
        // clock is wound back by hand.
        page.wind_caret_back(std::time::Duration::from_millis(600));
        assert!(
            caret_of(&mut page, &mut text).is_none(),
            "and gone half a second later"
        );

        // A keystroke puts it back on, whatever half of the blink it was in.
        page.typed("a");
        assert!(caret_of(&mut page, &mut text).is_some());
    }

    /// A field is one line long however much is typed into it: the line slides
    /// under the box so that the caret stays where the reader can see it.
    #[test]
    fn a_field_slides_its_text_to_keep_the_caret_in_sight() {
        let (mut page, mut text) = scene("<body><input size=6>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);

        // Short enough to fit: nothing has moved.
        page.typed("ab");
        let inside = caret_of(&mut page, &mut text).expect("a caret");
        let field = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .find(|&id| page.boxes().node(id).control.is_some())
            .expect("the field");
        assert_eq!(page.boxes().control_scroll(field).0, 0.0);

        // Far more than fits: the caret is still on screen and the text has moved.
        page.typed("cdefghijklmnopqrstuvwxyz");
        let after = caret_of(&mut page, &mut text).expect("a caret still");
        assert!(
            page.boxes().control_scroll(field).0 > 0.0,
            "the line slid under the box"
        );
        let rect = page.rect_of(field).expect("a rectangle");
        assert!(
            after.0 <= f64::from(rect.right()),
            "and the caret is inside it: {after:?} against {rect:?}"
        );
        assert!(after.0 > inside.0, "and past where it started");

        // Back to the start, and the field shows its first letter again.
        page.edit_text(EditAction::Home, false);
        caret_of(&mut page, &mut text);
        assert_eq!(page.boxes().control_scroll(field).0, 0.0);
    }

    /// Moving the caret changes nothing else about the page, so the frame would be
    /// reused unless the caret is part of what a frame is a function of.
    #[test]
    fn moving_the_caret_builds_a_new_frame() {
        let (mut page, mut text) = scene("<body><input value=abcdef>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        let before = page.builds();
        page.edit_text(EditAction::Right, false);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), before + 1);
    }

    #[test]
    fn nothing_is_typed_into_a_page_with_no_field_focused() {
        let (mut page, mut text) = scene("<body><p>text</p><input disabled>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(!page.typed("x"), "nothing has the focus");

        let (x, y) = point_on(&page, "input");
        page.pointer_pressed(x, y);
        assert!(!page.typed("x"), "and a disabled field never takes it");
    }

    fn glyph_ys(list: &DisplayList) -> Vec<f64> {
        let mut painter = RecordingPainter::new();
        render(list, &mut painter);
        painter
            .take()
            .iter()
            .filter_map(|op| match op {
                PaintOp::DrawGlyphs { transform, .. } => Some(transform.as_coeffs()[5]),
                _ => None,
            })
            .collect()
    }

    /// The colour of the first paragraph, which is what a media query in these
    /// tests changes.
    fn paragraph_colour(page: &PageScene) -> otlyra_gfx::peniko::Color {
        let boxes = page.boxes();
        boxes
            .descendants(boxes.root())
            .into_iter()
            .find(|&id| {
                boxes
                    .node(id)
                    .tag
                    .as_ref()
                    .is_some_and(|tag| tag.as_ref() == "p")
            })
            .map(|id| boxes.node(id).style.color)
            .expect("a paragraph")
    }

    /// A resize relays out; it re-cascades only when the viewport is something a
    /// rule reads.
    #[test]
    fn a_resize_restyles_only_when_a_rule_reads_the_viewport() {
        let (mut page, mut text) = scene(
            "<style>@media (min-width: 700px) { p { color: rgb(255, 0, 0) } }</style><p>text",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(
            paragraph_colour(&page),
            otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0)
        );

        page.build_display_list(&mut text, 500.0, 600.0, 0.0);
        assert_ne!(
            paragraph_colour(&page),
            otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0),
            "the query stopped matching and nothing noticed"
        );
    }

    /// A resize with nothing to restyle keeps the styles it had, and lays out
    /// again at the new width — which is the whole point of asking first.
    #[test]
    fn a_resize_nothing_reads_still_relays_out() {
        let (mut page, mut text) = scene("<style>p { color: rgb(0, 128, 0) }</style><p>text</p>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let colour = paragraph_colour(&page);

        page.build_display_list(&mut text, 300.0, 600.0, 0.0);
        assert_eq!(paragraph_colour(&page), colour);
        assert_eq!(
            page.layout.as_ref().expect("a layout").0,
            300.0,
            "laid out at the new width"
        );
    }

    /// A scrollbar can be taken hold of and dragged, and the content follows the
    /// thumb rather than the other way round.
    #[test]
    fn dragging_a_scrollbar_scrolls_the_page() {
        let (mut page, mut text) =
            scene("<style>body { margin: 0 } p { height: 3000px }</style><p>tall</p>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // Nowhere near the bar: the press is not for it.
        assert!(!page.grab_scrollbar(400.0, 300.0, 800.0, 600.0));

        // On the thumb, which sits at the top of a page that has not been scrolled.
        assert!(page.grab_scrollbar(795.0, 10.0, 800.0, 600.0));
        assert!(page.dragging_scrollbar());

        page.drag_scrollbar(300.0, 800.0, 600.0);
        let halfway = page.scroll();
        assert!(halfway > 0.0, "the drag did not move the page");

        page.drag_scrollbar(600.0, 800.0, 600.0);
        assert!(
            page.scroll() > halfway,
            "further down did not scroll further"
        );

        page.release_scrollbar();
        page.drag_scrollbar(0.0, 800.0, 600.0);
        assert!(page.scroll() > halfway, "it moved after being let go");
    }

    /// A scrolled panel's contents stay inside it. The regression this pins: the
    /// clip was decided by whether the contents fitted where the flow put them,
    /// which stopped being true the moment the panel scrolled — and the contents
    /// were then drawn over everything around the panel instead of under its edge.
    #[test]
    fn a_scrolled_panel_clips_what_it_has_moved() {
        let (mut page, mut text) = scene(
            "<style>body { margin: 0 } \
             .panel { overflow: hidden; height: 100px } \
             .item { height: 60px }</style>\
             <div class=panel><div class=item>a</div><div class=item>b</div>\
             <div class=item>c</div></div>",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        page.scroll_at(50.0, 50.0, 80.0);

        let list = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);

        // Everything drawn while a layer is open is inside it; the panel's own
        // rectangle is what that layer is.
        let mut depth = 0i32;
        let mut clipped_glyphs = 0;
        let mut loose_glyphs = 0;
        for op in painter.take() {
            match op {
                PaintOp::PushLayer { .. } => depth += 1,
                PaintOp::PopLayer => depth -= 1,
                PaintOp::DrawGlyphs { transform, .. } => {
                    let y = transform.as_coeffs()[5];
                    // The panel is the first hundred pixels of the page.
                    if !(0.0..=100.0).contains(&y) {
                        if depth > 0 {
                            clipped_glyphs += 1;
                        } else {
                            loose_glyphs += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        assert!(
            clipped_glyphs > 0,
            "the scroll moved nothing out of the panel, so this proves nothing"
        );
        assert_eq!(
            loose_glyphs, 0,
            "text scrolled out of the panel was drawn outside it"
        );
    }

    /// What a scrolled panel actually draws: the contents move, the box does not.
    #[test]
    fn scrolling_a_panel_moves_its_contents_and_not_its_edge() {
        let (mut page, mut text) = scene(
            "<style>body { margin: 0 } \
             .panel { overflow: hidden; height: 100px; background: rgb(0, 0, 255) } \
             .tall { height: 400px; background: rgb(255, 0, 0) }</style>\
             <div class=panel><div class=tall>inside</div></div>",
        );

        let tops = |page: &mut PageScene, text: &mut TextEngine| {
            let list = page.build_display_list(text, 800.0, 600.0, 0.0);
            let mut painter = RecordingPainter::new();
            render(&list, &mut painter);
            let mut panel = None;
            let mut inside = None;
            use otlyra_gfx::kurbo::Shape as _;
            for op in painter.take() {
                if let PaintOp::Fill { brush, shape, .. } = op {
                    if brush
                        == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                            0, 0, 255,
                        ))
                    {
                        panel = Some(shape.bounding_box().y0);
                    }
                    if brush
                        == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                            255, 0, 0,
                        ))
                    {
                        inside = Some(shape.bounding_box().y0);
                    }
                }
            }
            (panel.expect("the panel"), inside.expect("its contents"))
        };

        let (panel_before, inside_before) = tops(&mut page, &mut text);
        page.scroll_at(50.0, 50.0, 60.0);
        let (panel_after, inside_after) = tops(&mut page, &mut text);

        assert_eq!(panel_before, panel_after, "the box itself moved");
        assert_eq!(
            inside_before - inside_after,
            60.0,
            "its contents did not move by what the wheel said"
        );
    }

    /// A box that cuts its contents off and has more than it can show takes the
    /// wheel; the page takes it once that box has reached its end.
    #[test]
    fn a_scrollable_box_takes_the_wheel_before_the_page_does() {
        let (mut page, mut text) = scene(
            "<style>body { margin: 0 } \
             .panel { overflow: hidden; height: 100px } \
             .tall { height: 400px } \
             .after { height: 2000px }</style>\
             <div class=panel><div class=tall>inside</div></div>\
             <div class=after>after</div>",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // Over the panel: the panel scrolls and the page does not.
        page.scroll_at(50.0, 50.0, 60.0);
        assert_eq!(page.scroll(), 0.0, "the page moved instead of the panel");

        // Past the panel's end, the rest goes to the page.
        page.scroll_at(50.0, 50.0, 1000.0);
        page.scroll_at(50.0, 50.0, 40.0);
        assert!(page.scroll() > 0.0, "the panel kept the wheel to itself");

        // Below the panel, the page scrolls from the first turn.
        let was = page.scroll();
        page.scroll_at(50.0, 400.0, 30.0);
        assert!(page.scroll() > was);
    }

    #[test]
    fn the_title_names_the_tab_and_is_not_page_content() {
        let parsed = otlyra_html::parse(b"<title>A page</title><p>text", Some("utf-8"));
        assert_eq!(title_of(&parsed.document).as_deref(), Some("A page"));
    }

    #[test]
    fn a_document_reaches_the_paint_seam_as_glyphs() {
        let (mut scene, mut text) = scene("<body><h1>heading</h1><p>paragraph");
        let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(glyph_ys(&list).len(), 2, "the heading and the paragraph");
    }

    #[test]
    fn the_top_inset_moves_the_page_below_the_interface() {
        let (mut scene, mut text) = scene("<body><p>text");
        let flush = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));
        let inset = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 72.0));
        assert!((inset[0] - flush[0] - 72.0).abs() < 0.01);
    }

    #[test]
    fn scrolling_moves_the_page_up_and_is_clamped_to_the_content() {
        let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
        let (mut scene, mut text) = scene(&html);
        let before = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));

        scene.scroll_by(12.0);
        let after = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));
        assert!((before[0] - after[0] - 12.0).abs() < 0.01);

        scene.scroll_by(-1000.0);
        assert_eq!(scene.scroll(), 0.0);
    }

    /// A click into a field lands between the two letters it fell between, even
    /// when the page has been restyled since the last frame.
    ///
    /// The press is answered against the frame the reader was looking at. With the
    /// layout thrown away instead of marked out of date there was nothing to hit-
    /// test against, and every click into a field put the caret at the end of what
    /// it held — which is what focusing the field itself made happen.
    #[test]
    fn a_click_into_a_field_lands_where_it_fell_after_a_restyle() {
        let (mut page, mut text) = scene("<body><input value=\"Hello world\" size=30>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let field = page
            .boxes()
            .descendants(page.boxes().root())
            .into_iter()
            .find(|&id| page.boxes().node(id).control.is_some())
            .expect("the field");
        let rect = page.rect_of(field).expect("a rectangle");
        let y = f64::from(rect.y + rect.height / 2.0);

        // Focusing the field is itself a restyle, so this is the second click of
        // any pair — and it was the one that always missed.
        page.invalidate_styles();
        page.pointer_pressed(f64::from(rect.x) + 20.0, y);
        assert!(
            (1..=4).contains(&page.caret),
            "the caret landed at {} rather than near the start",
            page.caret
        );
    }

    /// A state change does not send the reader back to the top.
    ///
    /// Anything that restyles the page marks the layout out of date, and a scroll
    /// arriving before the next frame still has to know how far the page goes. When
    /// the layout was thrown away rather than marked, that question answered zero
    /// and the wheel snapped the page to the top — which is what a page full of
    /// controls did every time the pointer crossed one.
    #[test]
    fn a_restyle_before_the_next_frame_does_not_scroll_the_page_to_the_top() {
        let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
        let (mut scene, mut text) = scene(&html);
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        scene.scroll_by(500.0);
        assert_eq!(scene.scroll(), 500.0);

        // Something changed the page's style, and no frame has been drawn since.
        scene.invalidate_styles();
        scene.scroll_by(10.0);
        assert_eq!(
            scene.scroll(),
            510.0,
            "the wheel put the reader back at the top"
        );
    }

    #[test]
    fn a_page_shorter_than_the_window_cannot_scroll() {
        let (mut scene, mut text) = scene("<body><p>short");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        scene.scroll_by(500.0);
        assert_eq!(scene.scroll(), 0.0);
    }

    /// Scrolling must not relay out: layout is a function of the width, and the
    /// width has not changed.
    #[test]
    fn scrolling_reuses_the_layout_and_resizing_does_not() {
        let (mut scene, mut text) = scene("<body><p>text");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        scene.scroll_by(5.0);
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(scene.layout.as_ref().expect("laid out").0, 800.0);

        let _ = scene.build_display_list(&mut text, 400.0, 600.0, 0.0);
        assert_eq!(scene.layout.as_ref().expect("laid out").0, 400.0);
    }

    /// The assertion that keeps clicking honest: the link's target is the
    /// rectangle its text was drawn in, and nothing else on the page is.
    #[test]
    fn a_point_on_a_link_resolves_to_its_href() {
        let (mut scene, mut text) =
            scene("<body><p>before <a href=\"/next\">the link</a> after</p>");
        let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // Find where the link's own run was drawn, from the display list itself.
        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);
        let ops = painter.take();
        let blue = ops
            .iter()
            .filter_map(|op| match op {
                PaintOp::DrawGlyphs {
                    brush, transform, ..
                } if *brush
                    == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                        0, 0, 0xee,
                    )) =>
                {
                    Some(transform.as_coeffs())
                }
                _ => None,
            })
            .next()
            .expect("the link is painted in the UA blue");

        let (x, y) = (blue[4] + 4.0, blue[5] + 6.0);
        assert_eq!(scene.link_at(x, y).as_deref(), Some("/next"));
        assert_eq!(scene.link_at(x, y + 400.0), None, "below the text");
        assert_eq!(scene.link_at(2.0, y), None, "before the link starts");
    }

    #[test]
    fn a_link_around_other_elements_is_still_a_link() {
        let (mut scene, mut text) = scene("<body><p><a href=\"/x\"><b>bold link</b></a>");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // The narrowest target is the text run itself; the wide ones are the
        // blocks it sits inside.
        let hit = scene
            .targets
            .iter()
            .map(|(rect, _)| *rect)
            .min_by(|a, b| a.width().total_cmp(&b.width()))
            .expect("something was drawn");
        assert_eq!(
            scene.link_at(hit.x0 + 2.0, hit.y0 + 2.0).as_deref(),
            Some("/x"),
            "the text belongs to the <b>, and the link is above it"
        );
    }

    #[test]
    fn an_anchor_without_an_href_is_not_a_link() {
        let (mut scene, mut text) = scene("<body><p><a>not a link</a>");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(scene.link_at(10.0, 20.0), None);
    }

    #[test]
    fn an_empty_page_still_paints_its_canvas() {
        let (mut scene, mut text) = scene("");
        let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(matches!(
            list.items().first(),
            Some(DisplayItem::Fill { .. })
        ));
    }
}

/// Rendering tests: what the pipeline puts on a surface, read back as pixels.
///
/// A dump says what style an element computed; only pixels say whether the
/// difference reached the screen.
#[cfg(test)]
mod raster_tests {
    use super::*;

    /// How many non-white pixels each row of the rendered page has.
    fn ink_per_row(html: &str, width: u32, height: u32) -> Vec<u32> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let mut page = PageScene::new(document);
        // System fonts, not the vendored one: the vendored family has a single
        // static face, and a weight that no face can express is a weight no
        // rendering bug can be seen in.
        let mut text = otlyra_text::TextEngine::new();
        let list = page.build_display_list(&mut text, width as f32, height as f32, 0.0);

        let mut painter =
            otlyra_gfx::SkiaPainter::new_raster(width, height).expect("a raster surface");
        painter.clear(otlyra_gfx::peniko::Color::WHITE);
        otlyra_gfx::render(&list, &mut painter);
        let pixels = painter.read_rgba8().expect("read back");

        (0..height)
            .map(|y| {
                (0..width)
                    .filter(|x| {
                        let i = ((y * width + x) * 4) as usize;
                        pixels[i] < 200
                    })
                    .count() as u32
            })
            .collect()
    }

    /// Bold and regular of a variable font share one font file, so anything that
    /// caches by file alone hands the second run the first run's weight. The only
    /// way to see that is to draw both and count the ink.
    #[test]
    fn bold_text_is_heavier_than_regular_text_in_the_same_frame() {
        let ink = ink_per_row(
            "<style>p { font-size: 40px; margin: 0 } .b { font-weight: 700 }</style>\
             <p>iiiiiiii</p><p class=\"b\">iiiiiiii</p>",
            400,
            120,
        );

        let half = ink.len() / 2;
        let regular: u32 = ink[..half].iter().sum();
        let bold: u32 = ink[half..].iter().sum();
        assert!(regular > 0, "the regular line drew nothing");
        assert!(
            bold > regular,
            "bold inked {bold} pixels and regular {regular}"
        );
    }
}

/// A key that edits or moves the caret rather than adding a letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// Remove what is before the caret.
    Backspace,
    /// Remove what is after it.
    Delete,
    /// One character back.
    Left,
    /// One character on.
    Right,
    /// To the start.
    Home,
    /// To the end.
    End,
}

/// The byte before `at` that starts a character, or nothing at the start.
///
/// Characters rather than bytes, because a backspace that removes a byte of a
/// multi-byte letter leaves a string that is not text.
fn previous_boundary(value: &str, at: usize) -> Option<usize> {
    if at == 0 {
        return None;
    }
    value[..at]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
}

/// The byte after `at` that starts a character, or nothing at the end.
fn next_boundary(value: &str, at: usize) -> Option<usize> {
    let rest = value.get(at..)?;
    let mut chars = rest.chars();
    let next = chars.next()?;
    Some(at + next.len_utf8())
}

/// What the caret belongs to.
///
/// A field counts its offset in what the control holds; the page counts its in the
/// run of glyphs the last click landed in. Which of the two it is has to be
/// settled before the layout is borrowed, and where it lands after — so it is a
/// value rather than a rectangle.
#[derive(Clone, Copy, Debug)]
enum Caret {
    /// In a field, at a byte offset into its value.
    InField {
        /// The box the field generated.
        box_id: BoxId,
        /// How far into what it holds.
        offset: usize,
    },
    /// In the page's own text, where a collapsed selection is.
    InPage(otlyra_layout::TextPosition),
}

impl Caret {
    /// Where it is drawn.
    fn rect(self, fragments: &FragmentTree) -> Option<otlyra_layout::Rect> {
        match self {
            Self::InField { box_id, offset } => {
                otlyra_layout::selection::caret_in(fragments, box_id, offset)
            }
            Self::InPage(position) => otlyra_layout::selection::caret_rect(fragments, position),
        }
    }
}

/// The word around a byte offset: from the first character of it to past the last.
///
/// A word is a run of what is not white space, which is coarser than the page's
/// own idea of one and is what a field needs — there are no paragraphs in a field
/// and nothing to break a word across.
fn word_around(value: &str, at: usize) -> (usize, usize) {
    let at = at.min(value.len());
    let mut from = at;
    for (index, character) in value[..at].char_indices().rev() {
        if character.is_whitespace() {
            break;
        }
        from = index;
    }
    let mut to = at;
    for (index, character) in value[at..].char_indices() {
        if character.is_whitespace() {
            break;
        }
        to = at + index + character.len_utf8();
    }
    (from, to)
}

/// One attribute of one element.
fn attribute_of(document: &Document, id: NodeId, name: &str) -> Option<String> {
    document.get(id)?.element()?.attr(name).map(str::to_owned)
}

/// Every node under `root`, in tree order.
fn descendants_of(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(document.children(node));
    }
    order
}
