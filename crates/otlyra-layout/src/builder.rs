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

/// CSS `white-space: normal` collapsing.
///
/// A run of whitespace becomes one space, and a newline in the source is just more
/// whitespace — `<br>` is what makes a line break, not a line ending in the markup.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(character);
            in_space = false;
        }
    }
    out
}

/// The marker text for a list item, given the list it is in.
fn marker_text(ordered: bool, index: usize) -> String {
    if ordered {
        format!("{}. ", index + 1)
    } else {
        "• ".to_owned()
    }
}

impl Builder<'_> {
    /// The marker a list item should show, or `None` if it is not in a list we
    /// recognise.
    fn marker_for(&self, item: NodeId) -> Option<String> {
        let parent = self.document.get(item)?.parent?;
        let list = self.document.get(parent)?.element()?;
        let ordered = match list.name.local.as_ref() {
            "ol" => true,
            "ul" | "menu" => false,
            _ => return None,
        };

        // Counted over the items, not over every child: whitespace between them is
        // still text, and a numbered list that counts it numbers nothing.
        let index = self
            .document
            .children(parent)
            .filter(|&child| {
                self.document
                    .get(child)
                    .and_then(|node| node.element())
                    .is_some_and(|element| element.name.local.as_ref() == "li")
            })
            .position(|child| child == item)?;

        Some(marker_text(ordered, index))
    }

    /// The text a replaced or attribute-driven element shows.
    ///
    /// `None` for everything whose content is in the tree where it belongs.
    fn generated_text(&self, name: &str, node: NodeId) -> Option<String> {
        let attribute = |key: &str| {
            self.document
                .get(node)?
                .element()?
                .attrs
                .iter()
                .find(|attr| attr.name.local.as_ref() == key)
                .map(|attr| attr.value.to_string())
        };

        match name {
            "input" => {
                let kind = attribute("type").unwrap_or_else(|| "text".to_owned());
                match kind.to_ascii_lowercase().as_str() {
                    // A button-shaped input carries its label in `value`.
                    "button" | "submit" | "reset" => {
                        // Padded with spaces because an inline box has no padding
                        // here, and without them two buttons touch.
                        let label = attribute("value").unwrap_or_else(|| match kind.as_str() {
                            "submit" => "Submit".to_owned(),
                            "reset" => "Reset".to_owned(),
                            _ => " ".to_owned(),
                        });
                        Some(format!("  {label}  "))
                    }
                    // ASCII rather than the ballot-box and radio characters: those
                    // are dingbats many system fonts have no glyph for, and a
                    // missing glyph is a hollow box where the control should be.
                    "checkbox" => Some(if attribute("checked").is_some() {
                        "[x] ".to_owned()
                    } else {
                        "[ ] ".to_owned()
                    }),
                    "radio" => Some(if attribute("checked").is_some() {
                        "(o) ".to_owned()
                    } else {
                        "( ) ".to_owned()
                    }),
                    "hidden" => None,
                    // A text field shows its value, or its placeholder, or enough
                    // space to look like a field rather than vanishing.
                    _ => Some(
                        attribute("value")
                            .or_else(|| attribute("placeholder"))
                            .unwrap_or_else(|| "          ".to_owned()),
                    ),
                }
            }
            "img" => attribute("alt").filter(|alt| !alt.is_empty()),
            // The same padding a value-driven button gets, so that two buttons
            // side by side do not read as one.
            "button" => Some("  ".to_owned()),
            _ => None,
        }
    }

    /// Whether an `<option>` is the one a closed `<select>` displays.
    ///
    /// A select shows one option, not all of them — which is why a page full of
    /// dropdowns does not read as a wall of every choice in each.
    fn is_displayed_option(&self, option: NodeId) -> bool {
        let Some(parent) = self.document.get(option).and_then(|node| node.parent) else {
            return true;
        };
        let is_select = self
            .document
            .get(parent)
            .and_then(|node| node.element())
            .is_some_and(|element| element.name.local.as_ref() == "select");
        if !is_select {
            return true;
        }

        let options: Vec<NodeId> = self
            .document
            .children(parent)
            .filter(|&child| {
                self.document
                    .get(child)
                    .and_then(|node| node.element())
                    .is_some_and(|element| element.name.local.as_ref() == "option")
            })
            .collect();

        let selected = options.iter().find(|&&child| {
            self.document
                .get(child)
                .and_then(|node| node.element())
                .is_some_and(|element| {
                    element
                        .attrs
                        .iter()
                        .any(|attr| attr.name.local.as_ref() == "selected")
                })
        });

        match selected {
            Some(&chosen) => chosen == option,
            None => options.first() == Some(&option),
        }
    }

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
                // An option the select does not display generates no box, which is
                // `display: none` arrived at by a different route.
                if name == "option" && !self.is_displayed_option(node) {
                    return;
                }

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

                // A control's label is in an attribute, not in the tree: an
                // `<input>` is a void element, so without this it lays out as
                // nothing at all. Real browsers generate this content too — they
                // just do it inside a widget we do not have.
                if let Some(text) = self.generated_text(name, node) {
                    self.tree.push(
                        id,
                        BoxNode {
                            kind: BoxKind::Text(text.into()),
                            style: Arc::clone(&style),
                            node: None,
                            tag: None,
                            anonymous: true,
                            children: Vec::new(),
                            parent: None,
                        },
                    );
                }

                // A list item's marker. CSS generates it as a `::marker` box
                // outside the item's content; putting it inside the content is
                // coarser and visible in one place — a marker cannot sit in the
                // margin — but it is what makes a list look like a list.
                if name == "li"
                    && let Some(marker) = self.marker_for(node)
                {
                    self.tree.push(
                        id,
                        BoxNode {
                            kind: BoxKind::Text(marker.into()),
                            style: Arc::clone(&style),
                            node: None,
                            tag: None,
                            anonymous: true,
                            children: Vec::new(),
                            parent: None,
                        },
                    );
                }

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

                // Collapsed here, once per load, rather than in layout, which runs
                // again on every resize. The result cannot change between them: it
                // is a function of the text and of `white-space`, and neither is.
                let text = match parent_style.white_space {
                    otlyra_css::WhiteSpace::Pre => text.clone(),
                    otlyra_css::WhiteSpace::Normal => collapse_whitespace(text).into(),
                };

                self.tree.push(
                    parent_box,
                    BoxNode {
                        kind: BoxKind::Text(text),
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

    // An inline box containing a block becomes a block. CSS resolves this by
    // splitting the inline around the block and keeping both halves inline;
    // blockifying is coarser, and it is the difference between a page laying out and
    // a page collapsing into one enormous paragraph — `<center><table>` is on the
    // front page of Hacker News, and `<a>` wrapped around a `<div>` is legal HTML5
    // and everywhere.
    if children
        .iter()
        .any(|&child| tree.node(child).is_block_level())
    {
        tree.blockify(id);
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
