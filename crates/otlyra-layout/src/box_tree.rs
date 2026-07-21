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
    pub intrinsic: Option<(f32, f32)>,
}

/// One box.
#[derive(Clone, Debug)]
pub struct BoxNode {
    /// What it is.
    pub kind: BoxKind,
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
    pub fn is_block_level(&self) -> bool {
        matches!(self.kind, BoxKind::Block)
    }

    /// Whether this box is inline-level.
    pub fn is_inline_level(&self) -> bool {
        match &self.kind {
            BoxKind::Inline | BoxKind::Text(_) => true,
            // A replaced box takes the level its style gives it: an image is
            // inline by default and a block when a rule says so.
            BoxKind::Replaced(_) => self.style.display != otlyra_css::Display::Block,
            BoxKind::Block => false,
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
}

impl BoxTree {
    /// A tree with a single root box carrying `style`.
    pub fn new(style: Arc<ComputedStyle>) -> Self {
        let mut boxes = SlotMap::with_key();
        let root = boxes.insert(BoxNode {
            kind: BoxKind::Block,
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
        }
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
            style,
            node: None,
            tag: None,
            anonymous: true,
            children: Vec::new(),
            parent: None,
        })
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
