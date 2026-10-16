//! The arena, and everything you can ask it without changing it.

use std::sync::atomic::{AtomicU64, Ordering};

use html5ever::interface::QuirksMode;
use html5ever::ns;
use slotmap::SlotMap;

use crate::limits::DomLimits;
use crate::node::{Node, NodeData, NodeId};

/// Which document a node belongs to.
///
/// One number per document, never reused within a run. It exists so that a
/// handle to a node can say *whose* node it is: a script's wrapper carries one,
/// and a wrapper from another page finds nothing rather than finding the node
/// that happens to share its index here. Between two open pages that is a
/// security property, not a tidiness one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocumentId(u64);

impl DocumentId {
    /// The next one. Wrapping is not a concern: a browser that opened one
    /// document per nanosecond would take five hundred years to get there.
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The computed styles of one styling of a document, as script reads them.
///
/// A trait so this crate keeps no style engine: the cascade lives above it,
/// and hands down an answer, the way the geometry of the last frame is
/// handed down.
pub trait ComputedStyles: Send + Sync {
    /// `property`'s computed value on `node`, serialized, if `node` was
    /// styled and `property` names one.
    fn value(&self, node: NodeId, property: &str) -> Option<String>;
}

impl std::fmt::Debug for dyn ComputedStyles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ComputedStyles")
    }
}

/// Styles a document as it stands, for script that asks after changing it.
///
/// A browser answers `getComputedStyle` from the tree as script left it, not
/// from the last frame, and styles synchronously to do it. The cascade lives
/// above this crate, so it is handed down as this: whoever lends the document
/// to script says how to style it with the sheets it has.
pub trait StyleResolver {
    /// The computed styles of `document` as it is now, if it can be styled.
    fn resolve(&self, document: &Document) -> Option<std::sync::Arc<dyn ComputedStyles>>;
}

impl std::fmt::Debug for dyn StyleResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StyleResolver")
    }
}

/// A parsed document: one arena of nodes, plus the handful of facts that belong to
/// the document rather than to any node in it.
///
/// Reading is done through this type; every change goes through
/// [`DocumentMutator`](crate::DocumentMutator). Keeping the two apart is what lets
/// the HTML tree sink be a thin adapter rather than a second implementation of the
/// tree, so replacing the parser later does not mean rewriting the DOM.
#[derive(Debug)]
pub struct Document {
    id: DocumentId,
    nodes: SlotMap<NodeId, Node>,
    root: NodeId,
    quirks_mode: QuirksMode,
    limits: DomLimits,
    refused: usize,
    /// What the reader (or script) did to the document's controls: typed
    /// values, checkedness, selections. Kept with the tree so that the page's
    /// script and the browser read one answer.
    form: crate::form::FormState,
    /// What became of each element's picture, by element.
    images: std::collections::HashMap<NodeId, ImageState>,
    /// The element that has the focus, as the browser last settled it or
    /// script last asked.
    focused: Option<NodeId>,
    /// Where the window is scrolled to, in CSS pixels, as script reads it.
    scroll: (f64, f64),
    /// Each laid-out element's border box on the last frame, `[left, top,
    /// right, bottom]` in client coordinates, as script measures it.
    geometry: std::sync::Arc<std::collections::HashMap<NodeId, [f32; 4]>>,
    /// How many times the tree has been changed: what tells a styling of it
    /// from one of an older tree.
    mutations: u64,
    /// The computed styles script reads, and the mutation count of the tree
    /// they were computed from.
    computed: Option<(u64, std::sync::Arc<dyn ComputedStyles>)>,
    /// How to style the tree again when script reads a style after changing it.
    style_resolver: Option<std::rc::Rc<dyn StyleResolver>>,
}

/// What became of the picture an element asked for, and which address that
/// answer is for: script reads `complete` and `naturalWidth` from it, and an
/// element whose `src` has since changed is waiting again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageState {
    /// The address as the element spelled it.
    pub src: String,
    /// Its intrinsic size in pixels, or `None` when it failed to load or decode.
    pub size: Option<(u32, u32)>,
}

impl Document {
    /// An empty document: a root node and nothing else.
    pub fn new() -> Self {
        Self::with_limits(DomLimits::default())
    }

    /// An empty document with explicit limits.
    pub fn with_limits(limits: DomLimits) -> Self {
        let mut nodes = SlotMap::with_key();
        let root = nodes.insert(Node::new(NodeData::Document));
        Self {
            id: DocumentId::next(),
            nodes,
            root,
            quirks_mode: QuirksMode::NoQuirks,
            limits,
            refused: 0,
            form: crate::form::FormState::new(),
            images: std::collections::HashMap::new(),
            focused: None,
            scroll: (0.0, 0.0),
            geometry: std::sync::Arc::default(),
            mutations: 0,
            computed: None,
            style_resolver: None,
        }
    }

    /// Which document this is.
    pub fn id(&self) -> DocumentId {
        self.id
    }

    /// The document node. Never removed, so this handle is valid for the lifetime
    /// of the document.
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// The node behind a handle, or `None` if it has been removed.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// The node behind a handle.
    ///
    /// # Panics
    ///
    /// If the handle is stale. Use it only where the tree builder has just given us
    /// the handle and a stale one would mean the parser is broken.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// One attribute of the element `id`, by local name, in no namespace.
    ///
    /// `None` when `id` is stale, is not an element, or has no such attribute.
    /// No namespace, because that is where every attribute HTML defines is. A
    /// prefixed one — `xlink:href`, `xml:lang` — is put in a namespace of its own
    /// by the parser (HTML §13.2.6.1, "adjust foreign attributes") and is a
    /// different attribute that shares a local name, which a lookup by local
    /// name alone would mistake for the one asked for.
    pub fn attr(&self, id: NodeId, local_name: &str) -> Option<&str> {
        self.get(id)?.element()?.attr(local_name)
    }

    /// Where the element `id` links to, if it is a link; see
    /// [`ElementData::link_href`](crate::node::ElementData::link_href).
    pub fn link_href(&self, id: NodeId) -> Option<&str> {
        self.get(id)?.element()?.link_href()
    }

    /// The `href` of the base element that sets the document's base URL, if one
    /// does.
    ///
    /// HTML §4.2.3: the first HTML `base` element in tree order that has an
    /// `href`, wherever it is — one without the attribute sets only a default
    /// `target` and is passed over. The value is as written; resolving it against
    /// the document's own address is the frozen base URL, which needs that
    /// address and so is the caller's.
    pub fn base_element_href(&self) -> Option<&str> {
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if let Some(element) = self.get(id).and_then(Node::element)
                && element.name.ns == ns!(html)
                && &*element.name.local == "base"
                && let Some(href) = element.attr("href")
            {
                return Some(href);
            }
            let children: Vec<NodeId> = self.children(id).collect();
            stack.extend(children.into_iter().rev());
        }
        None
    }

    /// How many nodes the document holds.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the document holds nothing but its root.
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// The quirks mode the tree builder settled on.
    pub fn quirks_mode(&self) -> QuirksMode {
        self.quirks_mode
    }

    /// The limits this document enforces.
    pub fn limits(&self) -> DomLimits {
        self.limits
    }

    /// How many insertions were refused because they would have breached a limit.
    ///
    /// Non-zero means the tree is a truncation of the document, which is a thing a
    /// caller may reasonably want to say out loud.
    pub fn refused_insertions(&self) -> usize {
        self.refused
    }

    /// The element sibling before `id`, skipping text and comments.
    ///
    /// Selector matching walks siblings by element, because `p + p` means the
    /// paragraph before this one and not the whitespace between them.
    pub fn prev_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut current = self.get(id)?.prev_sibling();
        while let Some(candidate) = current {
            if self.get(candidate)?.element().is_some() {
                return Some(candidate);
            }
            current = self.get(candidate)?.prev_sibling();
        }
        None
    }

    /// The element sibling after `id`.
    pub fn next_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut current = self.get(id)?.next_sibling();
        while let Some(candidate) = current {
            if self.get(candidate)?.element().is_some() {
                return Some(candidate);
            }
            current = self.get(candidate)?.next_sibling();
        }
        None
    }

    /// The first element child of `id`.
    pub fn first_element_child(&self, id: NodeId) -> Option<NodeId> {
        self.children(id)
            .find(|&child| self.get(child).is_some_and(|node| node.element().is_some()))
    }

    /// Whether `id` has no children that count as content — no elements, and no
    /// text that is not whitespace. This is `:empty`.
    pub fn is_empty_element(&self, id: NodeId) -> bool {
        self.children(id)
            .all(|child| match self.get(child).map(|node| &node.data) {
                Some(NodeData::Element(_)) => false,
                Some(NodeData::Text(text)) => text.is_empty(),
                _ => true,
            })
    }

    /// The children of `id`, in tree order.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            document: self,
            next: self.nodes.get(id).and_then(Node::first_child),
        }
    }

    /// The distance from the root to `id`, counting the root as zero.
    ///
    /// Walks upward, and stops counting once past the depth limit — the answer is
    /// only ever used to compare against that limit.
    pub fn depth(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut current = self.nodes.get(id).and_then(|node| node.parent);
        while let Some(parent) = current {
            depth += 1;
            if depth > self.limits.max_depth {
                break;
            }
            current = self.nodes.get(parent).and_then(|node| node.parent);
        }
        depth
    }

    pub(crate) fn nodes_mut(&mut self) -> &mut SlotMap<NodeId, Node> {
        &mut self.nodes
    }

    pub(crate) fn set_quirks_mode(&mut self, mode: QuirksMode) {
        self.quirks_mode = mode;
    }

    /// `node`'s border box on the last frame, in client coordinates.
    pub fn client_rect(&self, node: NodeId) -> Option<[f32; 4]> {
        self.geometry.get(&node).copied()
    }

    /// The bottom-right corner of everything laid out, in client coordinates.
    pub fn laid_out_extent(&self) -> (f32, f32) {
        self.geometry
            .values()
            .fold((0.0, 0.0), |(right, bottom), rect| {
                (right.max(rect[2]), bottom.max(rect[3]))
            })
    }

    /// How many times the tree has been changed so far.
    pub fn mutations(&self) -> u64 {
        self.mutations
    }

    /// Note one change to the tree. Every [`crate::DocumentMutator`] is one.
    pub(crate) fn note_mutation(&mut self) {
        self.mutations += 1;
    }

    /// Hand over computed styles of the tree as it was at mutation count
    /// `at`: the last frame's, which script reads while the tree has not
    /// changed since.
    pub fn set_computed_styles(&mut self, at: u64, styles: std::sync::Arc<dyn ComputedStyles>) {
        self.computed = Some((at, styles));
    }

    /// Say how to style the tree again for script, or that it cannot be.
    pub fn set_style_resolver(&mut self, resolver: Option<std::rc::Rc<dyn StyleResolver>>) {
        self.style_resolver = resolver;
    }

    /// `property`'s computed value on `node`, serialized as CSSOM serializes
    /// it: what `getComputedStyle` reads.
    ///
    /// From the tree as it is: when it has changed since the styles were
    /// computed, it is styled again first, through the resolver, as a browser
    /// does when script asks. Without one, the last styles answer. `None` for
    /// an element that was not styled, or a property that is not one.
    pub fn computed_value(&mut self, node: NodeId, property: &str) -> Option<String> {
        let current = self
            .computed
            .as_ref()
            .is_some_and(|(at, _)| *at == self.mutations);
        if !current
            && let Some(resolver) = self.style_resolver.clone()
            && let Some(styles) = resolver.resolve(self)
        {
            self.computed = Some((self.mutations, styles));
        }
        self.computed.as_ref()?.1.value(node, property)
    }

    /// Hand over the last frame's element boxes.
    pub fn set_geometry(
        &mut self,
        geometry: std::sync::Arc<std::collections::HashMap<NodeId, [f32; 4]>>,
    ) {
        self.geometry = geometry;
    }

    /// Where the window is scrolled to, `(x, y)` in CSS pixels.
    pub fn scroll_position(&self) -> (f64, f64) {
        self.scroll
    }

    /// Say where the window is scrolled to.
    pub fn set_scroll_position(&mut self, scroll: (f64, f64)) {
        self.scroll = scroll;
    }

    /// The focused element, if one is.
    pub fn focused(&self) -> Option<NodeId> {
        self.focused
    }

    /// Say which element has the focus.
    pub fn set_focused(&mut self, node: Option<NodeId>) {
        self.focused = node;
    }

    /// What became of `node`'s picture, if anything has yet.
    pub fn image_state(&self, node: NodeId) -> Option<&ImageState> {
        self.images.get(&node)
    }

    /// Record what became of `node`'s picture.
    pub fn set_image_state(&mut self, node: NodeId, state: ImageState) {
        self.images.insert(node, state);
    }

    /// The state of the document's controls.
    pub fn form(&self) -> &crate::form::FormState {
        &self.form
    }

    /// The same, to change.
    pub fn form_mut(&mut self) -> &mut crate::form::FormState {
        &mut self.form
    }

    /// Reset the controls of `form`, or of the whole document, to their
    /// markup's values.
    pub fn reset_form(&mut self, form: Option<NodeId>) {
        let mut state = std::mem::take(&mut self.form);
        state.reset(self, form);
        self.form = state;
    }

    pub(crate) fn refuse(&mut self) {
        self.refused += 1;
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over a node's children, produced by [`Document::children`].
#[derive(Debug)]
pub struct Children<'a> {
    document: &'a Document,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let current = self.next?;
        self.next = self.document.get(current).and_then(Node::next_sibling);
        Some(current)
    }
}
