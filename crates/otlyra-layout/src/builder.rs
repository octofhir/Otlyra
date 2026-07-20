//! Building the box tree from a DOM and the UA stylesheet.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Display, has_renderable_children, initial_style, ua_style};
use otlyra_dom::{Document, NodeData, NodeId};

use crate::box_tree::{BoxId, BoxKind, BoxNode, BoxTree};

/// Build the box tree for `document`.
pub fn build_box_tree(document: &Document) -> BoxTree {
    let _span = tracing::info_span!("build_box_tree").entered();

    let root_style = Arc::new(initial_style());
    let tree = BoxTree::new(Arc::clone(&root_style));
    let root = tree.root();

    let mut builder = Builder { document, tree };
    for child in document.children(document.root()) {
        builder.walk(child, root, &root_style);
    }

    let mut tree = builder.tree;
    fix_anonymous_boxes(&mut tree, root);
    tracing::debug!(boxes = tree.len(), "box tree built");
    tree
}

struct Builder<'a> {
    document: &'a Document,
    tree: BoxTree,
}

impl Builder<'_> {
    fn walk(&mut self, node: NodeId, parent_box: BoxId, parent_style: &Arc<ComputedStyle>) {
        let Some(dom) = self.document.get(node) else {
            return;
        };

        match &dom.data {
            NodeData::Element(element) => {
                let name = element.name.local.as_ref();
                let style = Arc::new(ua_style(name, parent_style));

                // `display: none` generates no box, and neither do its descendants.
                // That is the whole of it: the subtree is not laid out, not painted,
                // and not hit-testable.
                let kind = match style.display {
                    Display::None => return,
                    Display::Block => BoxKind::Block,
                    Display::Inline => BoxKind::Inline,
                };

                let id = self.tree.push(
                    parent_box,
                    BoxNode {
                        kind,
                        style: Arc::clone(&style),
                        node: Some(node),
                        tag: Some(element.name.local.clone()),
                        anonymous: false,
                        children: Vec::new(),
                        parent: None,
                    },
                );

                if has_renderable_children(name) {
                    for child in self.document.children(node) {
                        self.walk(child, id, &style);
                    }
                }
            }

            NodeData::Text(text) => {
                // Whitespace between two blocks generates no box. The full rule is
                // subtler — it depends on `white-space` and on what sits either side
                // — and it arrives with inline layout.
                if text.trim().is_empty() {
                    return;
                }
                self.tree.push(
                    parent_box,
                    BoxNode {
                        kind: BoxKind::Text(text.clone()),
                        style: Arc::clone(parent_style),
                        node: Some(node),
                        tag: None,
                        anonymous: false,
                        children: Vec::new(),
                        parent: None,
                    },
                );
            }

            // Comments, doctypes and the document node itself generate nothing.
            _ => {
                for child in self.document.children(node) {
                    self.walk(child, parent_box, parent_style);
                }
            }
        }
    }
}

/// Wrap runs of inline children in anonymous block boxes, wherever a box has both
/// kinds of child.
///
/// This is the fixup that makes "a box's children are all block-level or all
/// inline-level" true, and that invariant is what lets block layout and inline
/// layout be two separate algorithms instead of one that constantly asks which case
/// it is in. `<div>text<p>para</p></div>` has one paragraph and one loose text node;
/// the text gets an anonymous block of its own.
pub(crate) fn fix_anonymous_boxes(tree: &mut BoxTree, id: BoxId) {
    let children = tree.node(id).children.clone();
    for &child in &children {
        fix_anonymous_boxes(tree, child);
    }

    let has_block = children
        .iter()
        .any(|&child| tree.node(child).is_block_level());
    let has_inline = children
        .iter()
        .any(|&child| tree.node(child).is_inline_level());
    if !(has_block && has_inline) {
        return;
    }

    // Anonymous boxes inherit from their parent and have no style of their own —
    // there is no element to have styled them.
    let style = Arc::new(ComputedStyle {
        display: Display::Block,
        ..ComputedStyle::inheriting_from(&tree.node(id).style)
    });

    let mut rebuilt: Vec<BoxId> = Vec::with_capacity(children.len());
    let mut run: Vec<BoxId> = Vec::new();

    for child in children {
        if tree.node(child).is_block_level() {
            flush_run(tree, &mut run, &mut rebuilt, &style);
            rebuilt.push(child);
        } else {
            run.push(child);
        }
    }
    flush_run(tree, &mut run, &mut rebuilt, &style);

    tree.set_children(id, rebuilt);
}

/// Move the pending inline run into one anonymous block.
fn flush_run(
    tree: &mut BoxTree,
    run: &mut Vec<BoxId>,
    rebuilt: &mut Vec<BoxId>,
    style: &Arc<ComputedStyle>,
) {
    if run.is_empty() {
        return;
    }
    let wrapper = tree.create_anonymous(BoxKind::Block, Arc::clone(style));
    let children = std::mem::take(run);
    tree.set_children(wrapper, children);
    rebuilt.push(wrapper);
}
