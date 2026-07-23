//! The box tree: what the DOM becomes once style has had its say.
//!
//! It is a separate tree, not an annotation on the DOM, because the two do not
//! correspond. One element can generate no box (`display: none`), one box
//! (the common case) or several; and boxes exist that no element generated at all
//! (anonymous boxes, below). Painting from the DOM is the shortcut that makes CSS
//! impossible later.

use std::sync::Arc;

use html5ever::LocalName;
use html5ever::tendril::StrTendril;
use otlyra_css::ComputedStyle;
use otlyra_dom::NodeId;
use otlyra_gfx::peniko::ImageData;
use slotmap::{SecondaryMap, SlotMap, new_key_type};

new_key_type! {
    /// A handle to a box in a [`BoxTree`].
    pub struct BoxId;
}

/// What kind of box this is.
#[derive(Clone, Debug, PartialEq)]
pub enum BoxKind {
    /// A block-level box: it stacks vertically and establishes or joins a block
    /// formatting context.
    Block,
    /// An inline-level box: it flows in a line.
    Inline,
    /// Text. A leaf, always inline-level.
    Text(StrTendril),
    /// A replaced box: its content comes from somewhere other than the document,
    /// and its size can come from the content itself.
    Replaced(Replaced),
}

/// What a replaced box shows, and how big it is when nothing says otherwise.
#[derive(Clone, Debug, PartialEq)]
pub struct Replaced {
    /// The decoded picture, absent while it is missing or failed to decode — in
    /// which case the box is still generated, at whatever size was asked for, the
    /// way a browser leaves room for a picture it has not got.
    pub image: Option<ImageData>,
    /// The size the content itself has, if it has one.
    ///
    /// The picture's own pixels, and nothing else. It is what the aspect ratio
    /// is taken from, so anything written into it that is not the content's own
    /// size changes the shape the picture is drawn at.
    pub intrinsic: Option<(f32, f32)>,
    /// What a `width` or `height` attribute asked for, if either did.
    ///
    /// A *presentational hint*, which is what HTML says these are: they act as
    /// if a rule of the lowest priority had set `width` and `height`, so a
    /// stylesheet overrides them and a missing one leaves the other to the
    /// aspect ratio. Kept beside the intrinsic size rather than folded into it —
    /// folded in, `width="40"` on a 4×2 picture makes the intrinsic size 40×2
    /// and the ratio twenty, and the picture is drawn two pixels tall.
    pub hint: (Option<f32>, Option<f32>),
}

/// A form control, as a box has to know it.
///
/// Not a kind of box. A control is a block container that happens to be drawn
/// with a widget behind it — a button holds its label, a field holds its value —
/// so what is recorded here is what layout and paint need *in addition* to the
/// box: how big the widget wants to be, and what it is.
///
/// The sizes are attributes rather than pixels, because pixels need the font and
/// the box tree is built before anything has been shaped. Turning `size="20"` into
/// a width is layout's answer, and it is the same answer HTML gives:
/// `(size − 1) × avg + max`.
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    /// What the control is, which decides what is drawn and how it is measured.
    pub kind: ControlKind,
    /// Whether the widget is drawn at all, or the element is an ordinary box.
    pub widget: bool,
    /// The `size` attribute of a text field or a `<select>`, when it has a usable
    /// one. Twenty is the default for a field, and it is applied here rather than
    /// carried as a `None` that every reader has to remember the default for.
    pub size: Option<u32>,
    /// A `<textarea>`'s `cols`, defaulting to twenty.
    pub cols: u32,
    /// A `<textarea>`'s `rows`, defaulting to two, and a list box's row count.
    pub rows: u32,
    /// What is drawn on top of the shape: a tick, a dot, a ring, a grey.
    pub state: ControlState,
    /// Whether a drop-down is showing its list.
    pub open: bool,
    /// Whether the widget's own size has already been written into the style.
    ///
    /// Layout runs many times over one box tree — every resize, every scroll that
    /// needs one — and the room a drop-down leaves for its arrow is *added* to the
    /// padding rather than replacing it. Without this it is added again on every
    /// pass, and the control grows twenty pixels a frame.
    pub sized: bool,
    /// How far the text inside has been slid out of sight, left and up.
    ///
    /// A field is one line long however much is typed into it and a text area is
    /// as many rows as it was asked for, so what moves is the text and not the
    /// box. Which way it has moved is a question about where the caret is, which
    /// only the thing holding the caret knows — so it is written in from outside
    /// rather than worked out here.
    pub scroll: (f32, f32),
}

/// The part of a control's state that changes what is drawn.
///
/// A copy of what the cascade already matched on, rather than a second answer to
/// the same questions: two answers to "is this checked" is how a tick ends up in a
/// box that no rule styled as checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlState {
    /// Ticked, or the selected member of its group.
    pub checked: bool,
    /// Neither ticked nor not: the dash rather than the tick.
    pub indeterminate: bool,
    /// Greyed, and not responding to anything.
    pub disabled: bool,
    /// Under the pointer.
    pub hovered: bool,
    /// Held down.
    pub active: bool,
    /// Focused, and the focus is to be shown.
    pub focus_ring: bool,
}

impl ControlKind {
    /// Whether this control is one the reader can open.
    #[must_use]
    pub fn opens(self) -> bool {
        matches!(self, Self::DropDown)
    }
}

/// What kind of control a box is.
///
/// Coarser than the `type` attribute on purpose: what is listed here are the
/// things that are *measured* or *drawn* differently. Every text-entry type is one
/// entry, because a `tel` field and a `url` field differ in what they accept and
/// in nothing a box can see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlKind {
    /// A field the reader types into: one line, measured from `size`.
    Field,
    /// A `<textarea>`: measured from `cols` and `rows`.
    Area,
    /// A button, however it was spelled. Measured by its label.
    Button,
    /// A checkbox: a square of a fixed size, with nothing in it that came from
    /// the document.
    Checkbox,
    /// A radio button: the same, drawn round.
    Radio,
    /// A drop-down `<select>`: as wide as its widest option, plus the arrow.
    DropDown,
    /// A `<select>` showing several rows at once.
    ListBox,
    /// A slider.
    Range,
    /// A colour well.
    Color,
    /// A file picker: a button with the chosen file's name beside it.
    File,
    /// A `<progress>` bar.
    Progress,
    /// A `<meter>`.
    Meter,
}

impl ControlKind {
    /// Whether the widget has a size of its own that nothing in the document can
    /// change — a checkbox is thirteen pixels because it is a checkbox.
    #[must_use]
    pub fn is_fixed_size(self) -> bool {
        matches!(self, Self::Checkbox | Self::Radio)
    }

    /// Whether the reader can type into it.
    #[must_use]
    pub fn is_text_entry(self) -> bool {
        matches!(self, Self::Field | Self::Area)
    }
}

/// One box.
#[derive(Clone, Debug)]
pub struct BoxNode {
    /// What it is.
    pub kind: BoxKind,
    /// What control it is, when it is one.
    ///
    /// Beside the kind rather than one of them: a control is still a block
    /// container, and every rule about how a block container is laid out applies
    /// to it unchanged.
    pub control: Option<Control>,
    /// Its computed style. Shared, because siblings usually agree.
    pub style: Arc<ComputedStyle>,
    /// The element or text node that generated it, absent for anonymous boxes.
    pub node: Option<NodeId>,
    /// The tag it came from. An interned atom, kept here so that dumping a tree, or
    /// asking whether a box is a `<br>`, does not need the document back.
    pub tag: Option<LocalName>,
    /// Whether this box was invented to keep the tree well-formed.
    pub anonymous: bool,
    /// Children, in order.
    pub children: Vec<BoxId>,
    /// The parent, absent for the root.
    pub parent: Option<BoxId>,
}

impl BoxNode {
    /// Whether this box is block-level.
    ///
    /// An `inline-block` is not: it is a block container, which is what is *inside*
    /// it, and it takes its place in a line like a word.
    pub fn is_block_level(&self) -> bool {
        matches!(self.kind, BoxKind::Block)
            && !matches!(
                self.style.display,
                otlyra_css::Display::InlineBlock | otlyra_css::Display::InlineFlex
            )
    }

    /// Whether this box is inline-level.
    pub fn is_inline_level(&self) -> bool {
        match &self.kind {
            BoxKind::Inline | BoxKind::Text(_) => true,
            // A replaced box takes the level its style gives it: an image is
            // inline by default and a block when a rule says so.
            BoxKind::Replaced(_) => self.style.display != otlyra_css::Display::Block,
            // `inline-block` and `inline-flex` are the boxes that are both: a
            // formatting context of their own that takes a place in a line rather
            // than a line of its own.
            BoxKind::Block => matches!(
                self.style.display,
                otlyra_css::Display::InlineBlock | otlyra_css::Display::InlineFlex
            ),
        }
    }
}

/// What a list item shows beside itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    /// The text of it: a counter with its full stop, or a bullet character.
    pub text: Arc<str>,
    /// Whether it marks the item rather than counting it.
    ///
    /// The two are placed differently and both are worth getting right: a counter
    /// is set as text and ends against the item's words, a bullet hangs further
    /// back than its own width would put it. Which it is cannot be recovered from
    /// the text, so it is carried.
    pub bullet: bool,
}

/// How many cells of the grid a table cell covers.
///
/// A cell is one column and one row unless it says otherwise, and one that says
/// otherwise stops being a cell of a column: a cell across two columns gives its
/// width to neither of them on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSpan {
    /// Columns, at least one.
    pub columns: usize,
    /// Rows, or zero for "every row left in the table" — which is what HTML's
    /// `rowspan="0"` means and what layout resolves once it knows how many there
    /// are.
    pub rows: usize,
}

impl Default for CellSpan {
    fn default() -> Self {
        Self {
            columns: 1,
            rows: 1,
        }
    }
}

/// An arena of boxes with one root.
#[derive(Debug)]
pub struct BoxTree {
    boxes: SlotMap<BoxId, BoxNode>,
    root: BoxId,
    /// Which box each DOM node generated. Sparse: most documents have nodes that
    /// generate none.
    by_node: SecondaryMap<NodeId, BoxId>,
    /// The marker text of each list item, for the few boxes that are one.
    ///
    /// Beside the tree rather than in the node, for the same reason `by_node` is:
    /// a marker belongs to a handful of boxes on a page and a field would cost
    /// every other box the space. It is not a child box because CSS does not make
    /// it one — a marker sits outside its item's content, and a box inside the
    /// content cannot.
    markers: SecondaryMap<BoxId, Marker>,
    /// How far each table cell reaches, for the few that reach past one cell.
    /// Sparse for the same reason `markers` is.
    spans: SecondaryMap<BoxId, CellSpan>,
    /// The columns a table declares, one style per column, for the few tables
    /// that declare any.
    ///
    /// `<col>` and `<colgroup>` generate no box — CSS makes them column boxes,
    /// which are not part of the box tree at all — so the style they carry has
    /// nowhere else to live. Expanded here: a `span` is already spread out, so a
    /// table with three columns has three entries whatever the markup wrote.
    columns: SecondaryMap<BoxId, Vec<Arc<ComputedStyle>>>,
}

impl BoxTree {
    /// A tree with a single root box carrying `style`.
    pub fn new(style: Arc<ComputedStyle>) -> Self {
        let mut boxes = SlotMap::with_key();
        let root = boxes.insert(BoxNode {
            kind: BoxKind::Block,
            control: None,
            style,
            node: None,
            tag: None,
            anonymous: false,
            children: Vec::new(),
            parent: None,
        });
        Self {
            boxes,
            root,
            by_node: SecondaryMap::new(),
            markers: SecondaryMap::new(),
            spans: SecondaryMap::new(),
            columns: SecondaryMap::new(),
        }
    }

    /// Record how far a table cell reaches.
    pub fn set_span(&mut self, id: BoxId, span: CellSpan) {
        self.spans.insert(id, span);
    }

    /// How far a table cell reaches: one column and one row unless it said so.
    pub fn span(&self, id: BoxId) -> CellSpan {
        self.spans.get(id).copied().unwrap_or_default()
    }

    /// Record the columns a table declared, in order and already expanded.
    pub fn set_columns(&mut self, id: BoxId, columns: Vec<Arc<ComputedStyle>>) {
        self.columns.insert(id, columns);
    }

    /// The columns a table declared. Empty for a table that declared none.
    pub fn columns(&self, id: BoxId) -> &[Arc<ComputedStyle>] {
        self.columns.get(id).map_or(&[], Vec::as_slice)
    }

    /// Give a list item the marker it shows.
    pub fn set_marker(&mut self, id: BoxId, marker: Marker) {
        self.markers.insert(id, marker);
    }

    /// The marker a box shows, if it is a list item that shows one.
    pub fn marker(&self, id: BoxId) -> Option<&Marker> {
        self.markers.get(id)
    }

    /// The root box.
    pub fn root(&self) -> BoxId {
        self.root
    }

    /// The box behind a handle.
    pub fn get(&self, id: BoxId) -> Option<&BoxNode> {
        self.boxes.get(id)
    }

    /// The box behind a handle.
    ///
    /// # Panics
    ///
    /// If the handle is stale.
    pub fn node(&self, id: BoxId) -> &BoxNode {
        &self.boxes[id]
    }

    /// How many boxes the tree holds.
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    /// Whether the tree is nothing but its root.
    pub fn is_empty(&self) -> bool {
        self.boxes.len() <= 1
    }

    /// The box a DOM node generated, if it generated one.
    pub fn box_for(&self, node: NodeId) -> Option<BoxId> {
        self.by_node.get(node).copied()
    }

    /// Add a box under `parent`.
    pub fn push(&mut self, parent: BoxId, node: BoxNode) -> BoxId {
        let dom_node = node.node;
        let id = self.boxes.insert(BoxNode {
            parent: Some(parent),
            ..node
        });
        self.boxes[parent].children.push(id);
        if let Some(dom_node) = dom_node {
            self.by_node.insert(dom_node, id);
        }
        id
    }

    /// Replace `parent`'s children wholesale. Used by the anonymous-box fixup,
    /// which rewrites a child list rather than editing it in place.
    pub(crate) fn set_children(&mut self, parent: BoxId, children: Vec<BoxId>) {
        for &child in &children {
            self.boxes[child].parent = Some(parent);
        }
        self.boxes[parent].children = children;
    }

    /// Replace the text a text box carries, or drop the box if what is left of
    /// it is nothing.
    ///
    /// White-space processing is the one thing that rewrites text after the tree
    /// is built, because what a space collapses to is a fact about the *run* it
    /// is in rather than about the node it came from.
    pub(crate) fn set_text(&mut self, id: BoxId, text: StrTendril) {
        if let BoxKind::Text(existing) = &mut self.boxes[id].kind {
            *existing = text;
        }
    }

    /// Remove `id` from its parent's children. The box itself stays in the arena
    /// with nothing pointing at it, which is what the rest of this tree does with
    /// a box it has replaced.
    pub(crate) fn detach(&mut self, id: BoxId) {
        let Some(parent) = self.boxes[id].parent else {
            return;
        };
        self.boxes[parent].children.retain(|&child| child != id);
    }

    /// Turn an inline-level box into a block-level one.
    ///
    /// See [`crate::builder`] for when and why: an inline box that contains a block
    /// is a shape CSS resolves by splitting the inline, and blockifying is the
    /// approximation we take until it does.
    pub(crate) fn blockify(&mut self, id: BoxId) {
        if matches!(self.boxes[id].kind, BoxKind::Inline) {
            self.boxes[id].kind = BoxKind::Block;
        }
    }

    /// Create a detached anonymous box of `kind` carrying `style`.
    pub(crate) fn create_anonymous(&mut self, kind: BoxKind, style: Arc<ComputedStyle>) -> BoxId {
        self.boxes.insert(BoxNode {
            kind,
            control: None,
            style,
            node: None,
            tag: None,
            anonymous: true,
            children: Vec::new(),
            parent: None,
        })
    }

    /// Note that a control's widget size has been settled.
    pub fn mark_sized(&mut self, id: BoxId) {
        if let Some(node) = self.boxes.get_mut(id)
            && let Some(control) = node.control.as_mut()
        {
            control.sized = true;
        }
    }

    /// Slide the text inside a control.
    pub fn set_control_scroll(&mut self, id: BoxId, scroll: (f32, f32)) {
        if let Some(node) = self.boxes.get_mut(id)
            && let Some(control) = node.control.as_mut()
        {
            control.scroll = scroll;
        }
    }

    /// How far the text inside a control has been slid.
    #[must_use]
    pub fn control_scroll(&self, id: BoxId) -> (f32, f32) {
        self.boxes
            .get(id)
            .and_then(|node| node.control.as_ref())
            .map_or((0.0, 0.0), |control| control.scroll)
    }

    /// Replace a box's computed style.
    ///
    /// For the one thing style cannot answer on its own: how big a widget is. A
    /// checkbox is thirteen pixels and a field is twenty characters wide, and both
    /// numbers need the font, which is not known when the tree is built. Writing
    /// them into the style is what makes every later question about the box's size
    /// — its own, its line's, its flex container's — get the same answer without
    /// each of them having to ask a second question first.
    pub fn set_style(&mut self, id: BoxId, style: Arc<ComputedStyle>) {
        if let Some(node) = self.boxes.get_mut(id) {
            node.style = style;
        }
    }

    /// Every box under `id`, depth first, `id` included.
    pub fn descendants(&self, id: BoxId) -> Vec<BoxId> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            out.push(current);
            let children = &self.boxes[current].children;
            stack.extend(children.iter().rev().copied());
        }
        out
    }
}

/// A [`BoxId`] as a plain number, and back.
///
/// The display list carries hit-test identifiers as `u64` because it is
/// serializable and crosses process boundaries later; a slotmap key is exactly a
/// number, and round-tripping it here keeps the conversion in one place with the
/// type it belongs to.
pub fn box_id_to_u64(id: BoxId) -> u64 {
    use slotmap::Key as _;
    id.data().as_ffi()
}

/// The inverse of [`box_id_to_u64`]. The id may be stale, which is what the
/// generation in it is for: looking it up returns `None` rather than a stranger.
pub fn box_id_from_u64(raw: u64) -> BoxId {
    BoxId::from(slotmap::KeyData::from_ffi(raw))
}

/// The invariant that makes block and inline layout separable: a box's children are
/// either all block-level or all inline-level, never both.
///
/// Returns the first box that breaks it. This is a checker rather than an assertion
/// inside the builder because it has to hold for trees the builder did not produce
/// — including, later, trees a DOM mutation has edited.
pub fn first_box_with_mixed_children(tree: &BoxTree) -> Option<BoxId> {
    tree.descendants(tree.root()).into_iter().find(|&id| {
        let children = &tree.node(id).children;
        let blocks = children
            .iter()
            .filter(|&&child| tree.node(child).is_block_level())
            .count();
        blocks != 0 && blocks != children.len()
    })
}

/// Why the box tree, or part of it, has to be rebuilt.
///
/// Named from the first day, and never merged into a bare boolean, because "why did
/// we rebuild four hundred times" has to stay answerable, and a boolean cannot
/// answer it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidationReason {
    /// A document was parsed or replaced.
    DocumentLoaded,
    /// The viewport changed size.
    ViewportResized,
    /// A node was inserted into the DOM.
    NodeInserted,
    /// A node was removed from the DOM.
    NodeRemoved,
    /// A node's text changed.
    TextChanged,
    /// An attribute changed, so style may have.
    AttributeChanged,
}
