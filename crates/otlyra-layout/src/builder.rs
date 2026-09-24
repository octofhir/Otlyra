//! Building the box tree from a DOM and the styles the cascade computed for it.
//!
//! The cascade is the only source of an element's style. There is no second,
//! built-in table to fall back on: the user-agent stylesheet is CSS, cascaded
//! with the page's own, and a box tree built without it would be a page styled
//! by rules no page can see or override.

use std::sync::Arc;

use html5ever::ns;
use otlyra_css::ComputedStyle;
use otlyra_css::Display;
use otlyra_css::cascade::{StyledDocument, Viewport};
use otlyra_dom::{Document, ElementData, FormState, NodeData, NodeId};

use crate::box_tree::{
    BoxId, BoxKind, BoxNode, BoxTree, CellSpan, Control, ControlKind, ControlState, Replaced,
};

/// Build the box tree for `document` using the styles the cascade computed for
/// it.
pub fn build_box_tree(document: &Document, styles: &StyledDocument) -> BoxTree {
    build(document, styles, &Images::default(), &FormState::new())
}

/// Build the box tree with the pictures the document's `<img>` elements asked for
/// already decoded.
///
/// One that has not arrived generates no replaced box, so the element falls back to
/// its `alt` text — which is what a browser shows while a picture is missing.
pub fn build_box_tree_with_images(
    document: &Document,
    styles: &StyledDocument,
    images: &Images,
) -> BoxTree {
    build(document, styles, images, &FormState::new())
}

/// Build the box tree knowing what the reader has typed into the page's controls.
///
/// A field shows its `value` attribute until somebody types into it and its own
/// value afterwards, which is HTML's dirty flag — so the box tree cannot be built
/// from the markup alone once the page has been used.
pub fn build_page_box_tree(
    document: &Document,
    styles: &StyledDocument,
    images: &Images,
    form: &FormState,
) -> BoxTree {
    build(document, styles, images, form)
}

/// The decoded pictures of a document, by the element that asked for each.
pub type Images = std::collections::HashMap<NodeId, Picture>;

/// A decoded picture, and how many of its own pixels go to one CSS pixel.
///
/// The density comes from the candidate that was chosen rather than from the
/// file: the same bytes are a picture of one size when a page asked for them at
/// `1x` and half that when it asked at `2x`.
#[derive(Clone, Debug)]
pub struct Picture {
    /// The pixels.
    pub data: otlyra_gfx::peniko::ImageData,
    /// The chosen candidate's density. Never zero.
    pub density: f32,
}

impl Picture {
    /// A picture at one device pixel per CSS pixel, which is what a plain `src`
    /// asks for.
    pub fn new(data: otlyra_gfx::peniko::ImageData) -> Self {
        Self { data, density: 1.0 }
    }
}

/// A picture a document asks for but does not contain.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageSource {
    /// The `<img>` element, which is how the decoded picture finds its way back to
    /// the box it belongs to.
    pub node: NodeId,
    /// The address, exactly as the attribute spells it: resolving it needs the
    /// document's own address, which this crate does not know.
    pub src: String,
    /// The density the chosen candidate is for, which is what the file's own
    /// size is divided by to get the picture's.
    pub density: f32,
}

/// Every picture the document asks for, in tree order.
///
/// One per element that shows one: an `<img>`, a `<video>`'s poster, an image
/// button, and an `<embed>` or an `<object>`, whose resource may turn out to be
/// a picture. Which file an `<img>` wants depends on the window: an element
/// offering several is asked here, before anything is fetched, because a
/// browser fetches the one it chose and not all of them.
pub fn image_sources(document: &Document, viewport: Viewport) -> Vec<ImageSource> {
    let mut sources = Vec::new();
    let mut stack = vec![document.root()];

    while let Some(id) = stack.pop() {
        if let Some(element) = document.get(id).and_then(|node| node.element())
            && let Some(source) = picture_source(document, id, element, viewport)
        {
            sources.push(source);
        }
        stack.extend(document.children(id).collect::<Vec<_>>().into_iter().rev());
    }

    sources
}

/// The picture one element asks for, if it is an element that shows one and
/// names it.
fn picture_source(
    document: &Document,
    id: NodeId,
    element: &ElementData,
    viewport: Viewport,
) -> Option<ImageSource> {
    // One file, at one of its pixels to a CSS pixel: nothing but an `<img>`
    // offers a choice.
    let named = |attribute: &str| {
        element.address(attribute).map(|src| ImageSource {
            node: id,
            src: src.to_owned(),
            density: 1.0,
        })
    };
    match (&element.name.ns, element.name.local.as_ref()) {
        (&ns!(html), "img") => crate::srcset::chosen(document, id, viewport)
            .filter(|chosen| !chosen.url.is_empty())
            .map(|chosen| ImageSource {
                node: id,
                src: chosen.url,
                density: chosen.density,
            }),
        (&ns!(html), "video") => named("poster"),
        (&ns!(html), "input") if otlyra_dom::form::is_image_button(document, id) => named("src"),
        (&ns!(html), "embed") => named("src"),
        (&ns!(html), "object") => named("data"),
        _ => None,
    }
}

fn build(
    document: &Document,
    styles: &StyledDocument,
    images: &Images,
    form: &FormState,
) -> BoxTree {
    let _span = tracing::info_span!("build_box_tree").entered();

    let tree = BoxTree::default();
    let root = tree.root();
    let root_style = Arc::clone(&tree.node(root).style);

    let mut builder = Builder {
        document,
        styles,
        images,
        form,
        tree,
    };
    for child in document.children(document.root()) {
        builder.walk(child, root, &root_style);
    }

    let mut tree = builder.tree;
    fix_anonymous_boxes(&mut tree, root);
    // After the anonymous boxes, because what a space collapses to depends on
    // what is beside it in its *formatting context*, and until the fixup has run
    // a run of inline content and the blocks around it are still one child list.
    collapse_white_space(&mut tree, root);
    tracing::debug!(boxes = tree.len(), "box tree built");
    tree
}

struct Builder<'a> {
    document: &'a Document,
    styles: &'a StyledDocument,
    images: &'a Images,
    /// What the reader has typed into the page's controls, which outranks what the
    /// markup says a control holds.
    form: &'a FormState,
    tree: BoxTree,
}

/// Remove the whitespace-only boxes that sit between block-level boxes.
///
/// The space in `</div> <div>` is not a word gap and generating a line box for it
/// would put a blank line between every pair of blocks. The space in
/// `</button> <button>` is the gap between two controls, and dropping it runs them
/// together — which is why this is decided here, where both neighbours are known,
/// rather than while walking the DOM.
fn drop_whitespace_between_blocks(tree: &mut BoxTree, id: BoxId) {
    let children = tree.node(id).children.clone();
    let is_space = |tree: &BoxTree, child: BoxId| {
        let node = tree.node(child);
        node.node.is_some()
            && node.style.white_space.collapses_spaces()
            && matches!(&node.kind, BoxKind::Text(text) if text.trim().is_empty())
    };
    let inline_neighbour = |tree: &BoxTree, child: Option<&BoxId>| {
        child.is_some_and(|&child| tree.node(child).is_inline_level() && !is_space(tree, child))
    };

    let kept: Vec<BoxId> = children
        .iter()
        .enumerate()
        .filter(|&(index, &child)| {
            if !is_space(tree, child) {
                return true;
            }
            inline_neighbour(tree, children.get(index.wrapping_sub(1)))
                || inline_neighbour(tree, children.get(index + 1))
        })
        .map(|(_, &child)| child)
        .collect();

    if kept.len() != children.len() {
        tree.set_children(id, kept);
    }
}

/// Rewrite the text a control's box shows, as the builder would have written it.
///
/// The one mutation of a built box tree there is, and it exists for one thing: a
/// reader typing. A field's text is generated content — an `<input>` is a void
/// element, so the value is put into an anonymous text box under the control the
/// way Blink puts it into an inner editor inside the input's shadow tree —  and
/// what a keystroke changes is that box's text and nothing else in the document.
///
/// The white-space pass is run again over the control, so what is stored is what
/// a rebuild would have stored rather than something that agrees with it until
/// somebody types two spaces.
///
/// Returns whether there was exactly one such text box to rewrite. There is not
/// when a field with no placeholder is empty — it generates no text box at all —
/// and that is a change of shape rather than of text, which the caller answers
/// by rebuilding.
pub fn set_generated_text(tree: &mut BoxTree, id: BoxId, text: &str) -> bool {
    let children = tree.node(id).children.clone();
    let mut texts = children
        .into_iter()
        .filter(|&child| matches!(tree.node(child).kind, BoxKind::Text(_)));
    let Some(only) = texts.next() else {
        return false;
    };
    if texts.next().is_some() {
        return false;
    }
    tree.set_text(only, text.into());
    collapse_white_space(tree, id);
    true
}

/// CSS white-space processing, over a whole inline formatting context at a time.
///
/// The unit is the context and not the text node, which is the whole of why this
/// is a pass rather than a line in the walk: `<span>a </span> <span>b</span>` is
/// three text nodes and one space, and no one of them can know that on its own.
/// Within a context, in document order:
///
/// - a run of collapsible spaces, tabs and line endings becomes one space;
/// - a collapsible space at the start of the context, or straight after a forced
///   break, is dropped, and so is one at its very end;
/// - a line ending is a space where `white-space` collapses them and a break
///   where it preserves them;
/// - preserved white space is emitted as it stands, and does not collapse what
///   comes after it.
///
/// What is left of a text box that came to nothing is nothing: the box goes,
/// rather than staying as an empty run for the shaper to be given.
fn collapse_white_space(tree: &mut BoxTree, id: BoxId) {
    let node = tree.node(id);
    // The context belongs to the *block container* whose lines these are. An
    // inline box inside it is walked through rather than treated as one of its
    // own — collapsing a `<span>` on its own would trim the space that joins it
    // to the span beside it, which is the one space the whole pass exists for.
    let contains_lines = matches!(node.kind, BoxKind::Block)
        && !node.children.is_empty()
        && node
            .children
            .iter()
            .all(|&child| tree.node(child).is_inline_level());

    if contains_lines {
        collapse_context(tree, id);
    }

    // Down either way: an `inline-block` inside a context is a context of its
    // own, and so is every block below a block.
    for child in tree.node(id).children.clone() {
        collapse_white_space(tree, child);
    }
}

/// One inline formatting context, collapsed.
fn collapse_context(tree: &mut BoxTree, root: BoxId) {
    let items = inline_items(tree, root);
    let mut state = Run::default();
    let mut written: Vec<(BoxId, String)> = Vec::new();
    // How far back a trim may reach. Anything before this is not at the end of
    // anything: something that is not text came after it, and the space in
    // `</button> <button>` is the gap between two controls rather than white
    // space trailing off the end of a line.
    let mut sealed = 0usize;

    for item in &items {
        match *item {
            Item::Text(id) => {
                let node = tree.node(id);
                let BoxKind::Text(text) = &node.kind else {
                    continue;
                };
                let collapsed = state.take(text, node.style.white_space);
                written.push((id, collapsed));
            }
            // A picture, an inline-block or a bordered inline is content: what
            // follows it is a word gap rather than the start of the context, and
            // what came before it is not trailing white space.
            Item::Content => {
                state.after_content();
                sealed = written.len();
            }
            Item::Break => {
                // Every browser drops the space in front of a forced break as
                // well as the one after it. Neither is ink, and a line that ends
                // in one is a line that ends where the words do.
                trim_trailing(&mut written[sealed..]);
                state.after_break();
                sealed = written.len();
            }
        }
    }

    // The end of the context is the end of the last line, so a space there is
    // trailing white space like any other.
    trim_trailing(&mut written[sealed..]);

    for (id, text) in written {
        if text.is_empty() {
            tree.detach(id);
        } else {
            tree.set_text(id, text.into());
        }
    }
}

/// Drop a collapsible space from the end of what has been written so far.
fn trim_trailing(written: &mut [(BoxId, String)]) {
    for (_, text) in written.iter_mut().rev() {
        if text.is_empty() {
            continue;
        }
        if text.ends_with(' ') {
            text.pop();
        }
        return;
    }
}

/// What a context holds, in the order the shaper will see it.
enum Item {
    /// A run of text.
    Text(BoxId),
    /// Something that is not text and takes room: a picture, an inline-block.
    Content,
    /// A `<br>`.
    Break,
}

/// The contents of one context, flattened.
///
/// Inline boxes are walked through — a `<span>` is the style on the text inside
/// it and not a thing of its own — and anything that establishes a context of its
/// own is one item, whatever is inside it.
fn inline_items(tree: &BoxTree, root: BoxId) -> Vec<Item> {
    let mut out = Vec::new();
    for &child in &tree.node(root).children {
        let node = tree.node(child);
        match &node.kind {
            BoxKind::Text(_) => out.push(Item::Text(child)),
            BoxKind::Replaced(_) => out.push(Item::Content),
            BoxKind::Block => out.push(Item::Content),
            BoxKind::Inline if node.tag.as_deref() == Some("br") => out.push(Item::Break),
            BoxKind::Inline => {
                let inside = inline_items(tree, child);
                if inside.is_empty() {
                    // An empty inline still has borders and padding, which take
                    // room and separate what is either side of them.
                    out.push(Item::Content);
                } else {
                    out.extend(inside);
                }
            }
        }
    }
    out
}

/// How far through a context the collapsing has got.
struct Run {
    /// Nothing has been emitted on this line yet, so a space would be leading.
    at_line_start: bool,
    /// The last thing emitted was a collapsible space, so another would be a
    /// second one.
    after_space: bool,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            at_line_start: true,
            after_space: false,
        }
    }
}

impl Run {
    /// Collapse one text box's characters, and carry the state on past it.
    fn take(&mut self, text: &str, white_space: otlyra_css::WhiteSpace) -> String {
        let mut out = String::with_capacity(text.len());
        for character in text.chars() {
            match character {
                '\n' if white_space.preserves_breaks() => {
                    // A break ends the line, so the spaces in front of it are
                    // trailing white space and go.
                    while out.ends_with(' ') && white_space.collapses_spaces() {
                        out.pop();
                    }
                    out.push('\n');
                    self.at_line_start = true;
                    self.after_space = false;
                }
                ' ' | '\t' | '\n' | '\r' if white_space.collapses_spaces() => {
                    if self.at_line_start || self.after_space {
                        continue;
                    }
                    out.push(' ');
                    self.after_space = true;
                }
                character => {
                    out.push(character);
                    // Preserved white space is white space that is *not*
                    // collapsible, so it neither starts a run nor continues one.
                    self.after_space = false;
                    self.at_line_start = false;
                }
            }
        }
        out
    }

    /// Something that is not text took room here.
    fn after_content(&mut self) {
        self.at_line_start = false;
        self.after_space = false;
    }

    /// A forced break: the next line starts empty.
    fn after_break(&mut self) {
        self.at_line_start = true;
        self.after_space = false;
    }
}

/// The marker text for one item, given the counter its list uses and its place in
/// it.
///
/// A number carries the `.` its counter style puts after it; a bullet is the
/// character alone, and where it sits is layout's question rather than this one's.
fn marker_text(style: otlyra_css::ListStyle, index: usize) -> Option<String> {
    use otlyra_css::ListStyle;

    Some(match style {
        ListStyle::None => return None,
        ListStyle::Disc => "\u{2022}".to_owned(),
        ListStyle::Circle => "\u{25e6}".to_owned(),
        ListStyle::Square => "\u{25aa}".to_owned(),
        ListStyle::Decimal => format!("{}.", index + 1),
        ListStyle::LowerAlpha => format!("{}.", alphabetic(index, false)),
        ListStyle::UpperAlpha => format!("{}.", alphabetic(index, true)),
        ListStyle::LowerRoman => format!("{}.", roman(index + 1).to_lowercase()),
        ListStyle::UpperRoman => format!("{}.", roman(index + 1)),
    })
}

/// The bijective base-26 counter: a…z, then aa…az, ba… — which is what CSS's
/// alphabetic counters are, and is not the same as writing the number in base 26.
fn alphabetic(index: usize, upper: bool) -> String {
    let first = if upper { b'A' } else { b'a' };
    let mut out = Vec::new();
    let mut n = index + 1;
    while n > 0 {
        n -= 1;
        out.push(first + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII letters")
}

/// Roman numerals, in the additive-subtractive form CSS specifies.
///
/// Above 3999 CSS says to fall back to decimal, which is what this does: there is
/// no numeral for four thousand that anybody agrees on.
fn roman(mut value: usize) -> String {
    const NUMERALS: [(usize, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];

    if !(1..4000).contains(&value) {
        return value.to_string();
    }
    let mut out = String::new();
    for (amount, numeral) in NUMERALS {
        while value >= amount {
            out.push_str(numeral);
            value -= amount;
        }
    }
    out
}

impl Builder<'_> {
    /// The marker a list item should show, or `None` if it shows none.
    ///
    /// Which counter it is comes from the item's own style, because
    /// `list-style-type` is inherited and the list is what sets it — so a page that
    /// changes it on the list, or on one item, is obeyed without this having to
    /// look at either.
    fn marker_for(&self, item: NodeId, style: &ComputedStyle) -> Option<crate::box_tree::Marker> {
        let parent = self.document.get(item)?.parent?;
        let list = self.document.get(parent)?.element()?;
        if !matches!(list.name.local.as_ref(), "ol" | "ul" | "menu") {
            return None;
        }

        // Counted over the items, not over every child: whitespace between them is
        // still text, and a numbered list that counts it numbers nothing. Only an
        // ordered list pays for the walk.
        let index = if style.list_style.is_ordered() {
            self.document
                .children(parent)
                .filter(|&child| {
                    self.document
                        .get(child)
                        .and_then(|node| node.element())
                        .is_some_and(|element| element.name.local.as_ref() == "li")
                })
                .position(|child| child == item)?
        } else {
            0
        };

        Some(crate::box_tree::Marker {
            text: marker_text(style.list_style, index)?.into(),
            bullet: !style.list_style.is_ordered(),
        })
    }

    /// The box an open drop-down's list goes in.
    ///
    /// Out of the flow and against the control's own padding box, which is what
    /// makes opening one move nothing on the page behind it — the same way every
    /// menu on the web is built, and for the same reason.
    fn open_list(&mut self, select: BoxId, style: &Arc<ComputedStyle>) -> BoxId {
        use otlyra_css::{Length, LengthOrAuto, MaxSize, Overflow, Position, Size};

        // The control itself becomes what the list is placed against. It goes on
        // cutting its own contents off at its edge — a field slides its line under
        // itself and a long option is cut short — because the list is not among
        // them: layout knows an open list when it sees one and leaves it alone.
        let mut anchor = (**style).clone();
        anchor.position = Position::Relative;
        self.tree.set_style(select, Arc::new(anchor));

        let mut list = (**style).clone();
        list.display = Display::Block;
        list.position = Position::Absolute;
        list.inset = otlyra_css::Sides {
            top: LengthOrAuto::Length(Length::Percent(1.0)),
            right: LengthOrAuto::Auto,
            bottom: LengthOrAuto::Auto,
            left: LengthOrAuto::ZERO,
        };
        list.width = Size::Auto;
        list.min_width = Size::Length(Length::Percent(1.0));
        list.height = Size::Auto;
        list.padding = otlyra_css::Sides::all(Length::Px(0.0));
        list.z_index = Some(1);
        // As tall as it needs and no taller than this: a list of two hundred
        // countries is a list two hundred rows long, and every browser caps it and
        // scrolls what is left. The number is a plain one for the same reason
        // theirs are.
        list.max_height = MaxSize::Length(Length::Px(300.0));
        list.overflow = Overflow::Clip;

        self.tree.push(
            select,
            BoxNode {
                kind: BoxKind::Block,
                // Not a widget — nothing is drawn for it and it is not measured
                // like one. It is a control so that it can be found and slid, which
                // is what a list too long to show has to do.
                control: Some(Control {
                    kind: ControlKind::ListBox,
                    widget: false,
                    size: None,
                    cols: 0,
                    rows: 0,
                    state: ControlState::default(),
                    position: None,
                    level: otlyra_dom::form::Level::default(),
                    swatch: None,
                    open: true,
                    natural: None,
                    scroll: (0.0, 0.0),
                }),
                style: Arc::new(list),
                node: None,
                tag: None,
                anonymous: true,
                children: Vec::new(),
                parent: None,
            },
        )
    }

    /// What control this element is, if it is one.
    ///
    /// The widget flag is the cascade's answer to `appearance`, and a control the
    /// page has turned off is still a control — it is still typed into, still
    /// checked, still submitted. What changes is that nothing is drawn for it and
    /// that it has no size of its own, which is why the flag travels with the
    /// description rather than replacing it.
    fn control_of(&self, node: NodeId) -> Option<Control> {
        use otlyra_dom::form::{Control as Semantic, InputKind};

        let semantic = Semantic::of(self.document, node)?;
        let kind = match semantic {
            Semantic::Input(input) => match input {
                InputKind::Checkbox => ControlKind::Checkbox,
                InputKind::Radio => ControlKind::Radio,
                InputKind::Range => ControlKind::Range,
                InputKind::Color => ControlKind::Color,
                InputKind::File => ControlKind::File,
                InputKind::Hidden => return None,
                _ if input.is_button() => ControlKind::Button,
                _ => ControlKind::Field,
            },
            Semantic::Button => ControlKind::Button,
            Semantic::Textarea => ControlKind::Area,
            Semantic::Select if otlyra_dom::form::is_list_box(self.document, node) => {
                ControlKind::ListBox
            }
            Semantic::Select => ControlKind::DropDown,
            Semantic::Progress => ControlKind::Progress,
            Semantic::Meter => ControlKind::Meter,
            Semantic::Option | Semantic::Optgroup | Semantic::Output | Semantic::Fieldset => {
                return None;
            }
        };

        let number = |key: &str| {
            self.document
                .attr(node, key)
                .and_then(|value| value.trim().parse::<u32>().ok())
                .filter(|&value| value > 0)
        };

        Some(Control {
            kind,
            widget: self.is_widget(node, kind),
            // Twenty characters is what a field is when nothing says otherwise —
            // a number from the specification, not a guess at a pleasant width.
            size: match kind {
                // A date field is as wide as the date it shows and no wider: its
                // width is the pattern's, not the twenty characters a text field
                // falls back to, and `size` means nothing on one.
                ControlKind::Field => Some(
                    self.temporal_width(node)
                        .unwrap_or_else(|| number("size").unwrap_or(20)),
                ),
                ControlKind::ListBox => Some(otlyra_dom::form::display_size(self.document, node)),
                _ => None,
            },
            cols: number("cols").unwrap_or(20),
            rows: number("rows").unwrap_or(2),
            state: self.control_state(node, kind),
            // What the widget is filled to. Read here rather than in the painter
            // because it is a question about the document — a `value`, a `max`, an
            // `optimum` — and the painter is given a box and a rectangle.
            position: match kind {
                ControlKind::Range => {
                    Some(otlyra_dom::form::range_position(self.document, self.form, node) as f32)
                }
                ControlKind::Progress => otlyra_dom::form::progress_position(self.document, node)
                    .map(|position| position as f32),
                ControlKind::Meter => {
                    Some(otlyra_dom::form::meter_reading(self.document, node).0 as f32)
                }
                _ => None,
            },
            level: match kind {
                ControlKind::Meter => otlyra_dom::form::meter_reading(self.document, node).1,
                _ => otlyra_dom::form::Level::default(),
            },
            swatch: (kind == ControlKind::Color)
                .then(|| otlyra_dom::form::color_value(self.document, self.form, node)),
            // A field is open when it is showing suggestions, and a field with
            // nothing to suggest is not open however hard it is pressed: an empty
            // list is a rectangle over the page with nothing in it.
            open: matches!(kind, ControlKind::DropDown | ControlKind::Field)
                && self
                    .styles
                    .state_of(node)
                    .contains(otlyra_css::state::ElementState::OPEN)
                && (kind != ControlKind::Field
                    || !otlyra_dom::form::suggestions_for(self.document, self.form, node)
                        .is_empty()),
            natural: None,
            scroll: (0.0, 0.0),
        })
    }

    /// Whether a control of `kind` is drawn as a widget.
    ///
    /// When the cascade still says `appearance: auto` and no author rule has
    /// taken the look away. A checkbox, a radio button and a slider are never
    /// taken away: the definition of `appearance` in CSS UI 4 calls them
    /// non-devolvable, and a page that gives a checkbox a background gets a
    /// checkbox with a background.
    fn is_widget(&self, node: NodeId, kind: ControlKind) -> bool {
        let auto = self
            .styles
            .style_of(node)
            .is_none_or(|values| otlyra_css::appearance::of(values).is_auto());
        let non_devolvable = matches!(
            kind,
            ControlKind::Checkbox | ControlKind::Radio | ControlKind::Range
        );
        auto && (non_devolvable || !self.styles.is_devolved(node))
    }

    /// The state a widget is drawn in: the same bits `:checked` and `:hover`
    /// were matched on, so the widget and the page's own rules cannot disagree.
    fn control_state(&self, node: NodeId, kind: ControlKind) -> ControlState {
        use otlyra_css::state::ElementState;

        let bits = self.styles.state_of(node);
        ControlState {
            checked: bits.contains(ElementState::CHECKED),
            indeterminate: kind == ControlKind::Checkbox
                && bits.contains(ElementState::INDETERMINATE),
            disabled: bits.contains(ElementState::DISABLED),
            hovered: bits.contains(ElementState::HOVER),
            active: bits.contains(ElementState::ACTIVE),
            focus_ring: bits.contains(ElementState::FOCUSRING),
        }
    }

    /// How many characters a date or a time field shows, if it is one.
    fn temporal_width(&self, node: NodeId) -> Option<u32> {
        let otlyra_dom::form::Control::Input(kind) =
            otlyra_dom::form::Control::of(self.document, node)?
        else {
            return None;
        };
        let (pattern, _) = otlyra_dom::form::temporal_pattern(kind)?;
        Some(pattern.chars().count() as u32)
    }

    /// The text a control shows that is not in the document.
    ///
    /// An `<input>` is a void element, so without this a field and a button lay
    /// out as nothing at all. Browsers generate this content too — they just do it
    /// inside a widget, which is why a checkbox generates none of it: what a
    /// checkbox shows is a tick, and a tick is drawn rather than set.
    fn generated_text(&self, name: &str, node: NodeId) -> Option<String> {
        use otlyra_dom::form::{self, InputKind};

        let attribute = |key: &str| self.document.attr(node, key);
        // What a picture that is not there is shown as (§15.4.2).
        let alternative = || {
            attribute("alt")
                .filter(|alt| !alt.is_empty())
                .map(str::to_owned)
        };

        match name {
            "input" => {
                let kind = attribute("type").map_or(InputKind::Text, InputKind::parse);
                match kind {
                    // A button-shaped input carries its label in `value`. The two
                    // that have a label without one have it because HTML says so.
                    InputKind::Submit => Some(attribute("value").unwrap_or("Submit").to_owned()),
                    InputKind::Reset => Some(attribute("value").unwrap_or("Reset").to_owned()),
                    InputKind::Button => attribute("value").map(str::to_owned),
                    // Drawn, not set.
                    InputKind::Checkbox
                    | InputKind::Radio
                    | InputKind::Range
                    | InputKind::Color
                    | InputKind::Hidden => None,
                    InputKind::Image => alternative(),
                    // What it holds, or that it holds nothing. Never where the
                    // file came from: a page is told a name and not a path.
                    InputKind::File => Some(form::file_label(self.form, node)),
                    // A date or a time is not one string being typed into: it
                    // shows what has been filled in over the shape of what has
                    // not, so that two more digits are obviously wanted.
                    _ if form::temporal_pattern(kind).is_some() => {
                        form::temporal_display(self.document, self.form, node)
                    }
                    // A field shows what it holds, or the hint it was given. What
                    // it holds is what the reader typed once the reader has typed:
                    // the attribute is only the value until then. An empty one
                    // shows nothing and is still as wide as it is — the width is
                    // the widget's, not the text's.
                    _ => {
                        let held = self.form.value(self.document, node);
                        if held.is_empty() {
                            attribute("placeholder").map(str::to_owned)
                        } else {
                            Some(held.to_owned())
                        }
                    }
                }
            }
            // A closed drop-down shows one option, and the options themselves
            // generate no boxes — see where the children are walked.
            "select" if !form::is_list_box(self.document, node) => {
                let state = self.form;
                let options = form::options_of(self.document, node);
                let chosen = options
                    .iter()
                    .copied()
                    .find(|&option| state.selectedness(self.document, option))
                    .or_else(|| options.first().copied())?;
                Some(self.text_of(chosen))
            }
            // A suggestion is written as `<option value=Berlin>` more often than
            // not, and an option with nothing between its tags shows nothing. What
            // it offers to put in the field is what it has to show, so an empty one
            // shows its label or, failing that, the value itself.
            "option"
                if self.text_of(node).trim().is_empty()
                    && form::is_suggestion(self.document, node) =>
            {
                Some(
                    attribute("label")
                        .map_or_else(|| form::option_value(self.document, node), str::to_owned),
                )
            }
            // A text area's value is its content, so it needs no generated text —
            // until the reader types, after which its content is no longer what it
            // holds and the children are skipped in favour of this.
            "textarea" if self.form.is_dirty(node) => {
                Some(self.form.value(self.document, node).to_owned())
            }
            "img" => alternative(),
            _ => None,
        }
    }

    /// All the text under a node, run together.
    fn text_of(&self, node: NodeId) -> String {
        let mut out = String::new();
        let mut stack: Vec<NodeId> = self.document.children(node).collect();
        stack.reverse();
        while let Some(id) = stack.pop() {
            match self.document.get(id).map(|inner| &inner.data) {
                Some(NodeData::Text(text)) => out.push_str(text),
                _ => {
                    let mut children: Vec<NodeId> = self.document.children(id).collect();
                    children.reverse();
                    stack.extend(children);
                }
            }
        }
        out.trim().to_owned()
    }

    /// The replaced content an element shows, if it is a replaced element
    /// (HTML §15.4).
    ///
    /// Each of HTML's embedded elements says what it shows and how big that is.
    /// Nested documents, plug-ins and the frames of a video or a sound are not
    /// shown here: a frame, an `embed` and a video are drawn as the boxes a
    /// reference lays them out as, with nothing in them but a poster.
    fn replaced_content(&self, element: &ElementData, node: NodeId) -> Option<Replaced> {
        match (&element.name.ns, element.name.local.as_ref()) {
            // A picture, once it has arrived. Until then the element is its
            // alternative text, which is the whole point of having one
            // (§15.4.2).
            (&ns!(html), "img") => self.picture(node),
            (&ns!(html), "input") if otlyra_dom::form::is_image_button(self.document, node) => {
                self.picture(node)
            }
            // The poster, whose size is the video's while no frame of the video
            // has been decoded, which none is; and before it, or without one,
            // nothing, at the default object size (§15.4.1, §4.8.9).
            (&ns!(html), "video") => Some(self.picture(node).unwrap_or(Replaced::EMPTY)),
            // A bitmap as big as the element's attributes say (§4.12.5.1), and
            // transparent: nothing draws into it without the scripting API.
            (&ns!(html), "canvas") => Some(Replaced {
                image: None,
                intrinsic: Some(canvas_size(element)),
            }),
            (&ns!(html), "iframe") => Some(Replaced::EMPTY),
            // A plug-in, or the picture it turned out to be. One with no `src`
            // represents nothing (§4.8.6), and both references give it no box.
            (&ns!(html), "embed") if element.address("src").is_some() => {
                Some(self.picture(node).unwrap_or(Replaced::EMPTY))
            }
            // The picture its `data` is, once it has arrived. An `object`
            // whose `data` is anything else — or that has none — shows its
            // children, which are there for exactly that (§4.8.7): HTML has no
            // plug-ins any more, and a nested document is not shown here.
            (&ns!(html), "object") => self.picture(node),
            // Its controls, which are not drawn: the room they take is the
            // user-agent sheet's, and content that shows nothing has no size
            // of its own. An `audio` that exposes no controls is never shown,
            // whatever a page's rules say, so this is one that does (§15.4.1).
            (&ns!(html), "audio") => Some(Replaced {
                image: None,
                intrinsic: Some((0.0, 0.0)),
            }),
            _ => None,
        }
    }

    /// The picture an element asked for, as replaced content, once it has
    /// arrived.
    fn picture(&self, node: NodeId) -> Option<Replaced> {
        let picture = self.images.get(&node)?.clone();
        // The file's own size divided by the density it was chosen for: a
        // picture picked at two device pixels per CSS pixel is drawn at half its
        // width, which is the whole point of asking for a denser one.
        let density = picture.density.max(f32::MIN_POSITIVE);
        let intrinsic = (
            picture.data.width as f32 / density,
            picture.data.height as f32 / density,
        );
        Some(Replaced {
            image: Some(picture.data),
            intrinsic: Some(intrinsic),
        })
    }

    /// How far a `<td>` or `<th>` reaches, from its `colspan` and `rowspan`.
    ///
    /// HTML's own limits: a column span is between one and a thousand, a row span
    /// at most 65534, and anything that is not a number at all is one. A row span
    /// of zero is the exception that means something — every row left in the table
    /// — and is carried through as zero for layout to resolve.
    fn span_of(&self, node: NodeId) -> CellSpan {
        let number =
            |key: &str| -> Option<usize> { self.document.attr(node, key)?.trim().parse().ok() };

        CellSpan {
            columns: number("colspan").unwrap_or(1).clamp(1, 1000),
            rows: number("rowspan").unwrap_or(1).min(65534),
        }
    }

    /// The columns a table declares, in order and with every `span` spread out.
    ///
    /// A `<col>` inside a `<colgroup>` describes one column; a `<colgroup>` with
    /// no `<col>` in it describes as many as its own `span` says. A group's style
    /// is not inherited by the columns inside it — they are siblings in the
    /// column list, not boxes inside one another — so a group with columns in it
    /// contributes those and nothing of its own.
    fn columns_of(&self, table: NodeId) -> Vec<Arc<ComputedStyle>> {
        let named = |node: NodeId, name: &str| {
            self.document
                .get(node)
                .and_then(|node| node.element())
                .is_some_and(|element| element.name.local.as_ref() == name)
        };

        let mut columns = Vec::new();
        for child in self.document.children(table) {
            if named(child, "col") {
                self.spread_column(child, &mut columns);
            } else if named(child, "colgroup") {
                let inner: Vec<NodeId> = self
                    .document
                    .children(child)
                    .filter(|&node| named(node, "col"))
                    .collect();
                if inner.is_empty() {
                    self.spread_column(child, &mut columns);
                }
                for col in inner {
                    self.spread_column(col, &mut columns);
                }
            }
        }
        columns
    }

    /// The columns one `<col>`, or one `<colgroup>` with none inside it,
    /// describes: as many as its `span` says, every one in its style.
    ///
    /// HTML's own limits, and its own default: a span is between one and a
    /// thousand, and anything that is not a number at all is one.
    fn spread_column(&self, node: NodeId, into: &mut Vec<Arc<ComputedStyle>>) {
        let Some(style) = self.style_for(node) else {
            return;
        };
        let span = self
            .document
            .attr(node, "span")
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 1000);
        into.extend(std::iter::repeat_n(Arc::new(style), span));
    }

    /// The style the cascade computed for one element.
    ///
    /// `None` for an element the cascade did not reach — one put into the
    /// document after its styles were computed — and such an element generates
    /// no box until the restyle its insertion called for. Nothing is invented in
    /// its place: a style made up here would be a second user-agent stylesheet,
    /// and one that no page could see or override.
    fn style_for(&self, node: NodeId) -> Option<ComputedStyle> {
        self.styles
            .style_of(node)
            .map(|values| otlyra_css::computed::to_layout_style(values))
    }

    fn walk(&mut self, node: NodeId, parent_box: BoxId, parent_style: &Arc<ComputedStyle>) {
        let Some(dom) = self.document.get(node) else {
            return;
        };

        match &dom.data {
            NodeData::Element(element) => {
                let name = element.name.local.as_ref();
                let Some(style) = self.style_for(node) else {
                    return;
                };
                let style = Arc::new(style);

                // `display: none` generates no box, and neither do its descendants.
                // That is the whole of it: the subtree is not laid out, not painted,
                // and not hit-testable.
                if style.display == Display::None {
                    return;
                }

                let kind = match self.replaced_content(element, node) {
                    Some(content) => BoxKind::Replaced(content),
                    None => match style.display {
                        Display::None => return,
                        Display::Inline => BoxKind::Inline,
                        // A flex or grid container, a table and every part of one
                        // are block-level boxes. What makes them more than that is
                        // their style, which layout reads when it gets to their
                        // children.
                        _ => BoxKind::Block,
                    },
                };

                let control = self.control_of(node);
                let id = self.tree.push(
                    parent_box,
                    BoxNode {
                        kind,
                        control,
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
                // A replaced box shows its content, not its stand-in: the `alt`
                // text is what is shown *instead* of a picture, not beside it.
                let replaced = matches!(self.tree.node(id).kind, BoxKind::Replaced(_));
                let generated = (!replaced)
                    .then(|| self.generated_text(name, node))
                    .flatten();
                if let Some(text) = generated {
                    self.tree.push(
                        id,
                        BoxNode {
                            kind: BoxKind::Text(text.into()),
                            control: None,
                            style: Arc::clone(&style),
                            node: None,
                            tag: None,
                            anonymous: true,
                            children: Vec::new(),
                            parent: None,
                        },
                    );
                }

                // A list item's marker, recorded on the item rather than pushed
                // into it: CSS puts a `::marker` outside its item's content, and a
                // box inside the content cannot be outside it. Layout places it
                // against the item's first line.
                if name == "li"
                    && let Some(marker) = self.marker_for(node, &style)
                {
                    self.tree.set_marker(id, marker);
                }

                // How far a cell reaches is markup rather than style: there is no
                // property for it, so layout has to be told here or not at all.
                if matches!(name, "td" | "th") {
                    let span = self.span_of(node);
                    if span != CellSpan::default() {
                        self.tree.set_span(id, span);
                    }
                }

                // The columns a table declares. Recorded on the table because a
                // `<col>` generates no box of its own — CSS makes it a column box,
                // which is not part of this tree — and its style has nowhere else
                // to go.
                if name == "table" {
                    let columns = self.columns_of(node);
                    if !columns.is_empty() {
                        self.tree.set_columns(id, columns);
                    }
                }

                // A closed drop-down shows one option and not the list. Its
                // options are still there and still selectable; what they are not
                // is boxes, because a box for each of them is what a list box is.
                let control = self.tree.node(id).control.clone();
                let closed = control
                    .as_ref()
                    .is_some_and(|control| control.kind == ControlKind::DropDown)
                    || (name == "textarea" && self.form.is_dirty(node));
                // An open drop-down shows its list *over* the page rather than in
                // it: the options go into a box of their own, placed against the
                // control and out of the flow, so opening one moves nothing.
                //
                // A field showing suggestions shows the same kind of list, and the
                // options in it come from a `<datalist>` somewhere else in the
                // document — which is why the list is filled from what the control
                // suggests rather than from what it holds.
                let popup = control
                    .as_ref()
                    .filter(|control| control.open)
                    .map(|_| self.open_list(id, &style));
                if let Some(popup) = popup {
                    let contents = if control
                        .as_ref()
                        .is_some_and(|control| control.kind == ControlKind::Field)
                    {
                        otlyra_dom::form::suggestions_for(self.document, self.form, node)
                    } else {
                        self.document.children(node).collect::<Vec<_>>()
                    };
                    for child in contents {
                        self.walk(child, popup, &style);
                    }
                } else if !replaced && has_renderable_children(element) && !closed {
                    for child in self.document.children(node) {
                        self.walk(child, id, &style);
                    }
                }
            }

            NodeData::Text(text) => {
                // The text exactly as it was written. What its spaces come to is
                // decided once the whole tree is built, because collapsing is a
                // fact about the run a space is in and not about the node it came
                // from: the space between `</span>` and `<span>` is the same
                // space as the one that ends the first of them.
                self.tree.push(
                    parent_box,
                    BoxNode {
                        kind: BoxKind::Text(text.clone()),
                        control: None,
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

/// Whether an element's children are content at all, whatever its style says.
///
/// Separate from `display: none` because the reason differs: what is inside a
/// `<script>` or a `<style>` is program source rather than text, and what is
/// inside an `<iframe>` is a document of its own. What is inside a `<video>`,
/// an `<audio>` or a `<canvas>` is fallback for a browser that cannot play or
/// draw them (§4.8.9, §4.12.5), which is not this one. A rule that makes the
/// element itself visible does not make any of that prose.
///
/// An `<object>` is not among them: its children are what it shows whenever
/// it is not replaced (§4.8.7), and a replaced box has no children anyway.
/// HTML's elements only — the names mean this in the HTML namespace and nothing
/// in any other, so an SVG or MathML element that happens to share one has
/// children like any other element.
fn has_renderable_children(element: &ElementData) -> bool {
    element.name.ns != ns!(html)
        || !matches!(
            element.name.local.as_ref(),
            "script" | "style" | "template" | "noscript" | "iframe" | "video" | "audio" | "canvas"
        )
}

/// A `<canvas>`'s bitmap size (§4.12.5.1): its `width` and `height`, read as
/// non-negative integers, and three hundred by a hundred and fifty wherever one
/// is missing or does not parse.
///
/// The bitmap's size and not the element's: CSS sizes the element, and the
/// bitmap is what it keeps the shape of.
fn canvas_size(canvas: &ElementData) -> (f32, f32) {
    let side = |name: &str, default: u32| {
        canvas
            .attr(name)
            .and_then(otlyra_css::non_negative_integer)
            .unwrap_or(default) as f32
    };
    (side("width", 300), side("height", 150))
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
    drop_whitespace_between_blocks(tree, id);

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

    // Every child of a flex container is a flex item, and a run of inline content
    // between two of them is one item of its own — so a container with any inline
    // child needs the same wrapping a mixed block does.
    let flex = matches!(
        tree.node(id).style.display,
        Display::Flex | Display::InlineFlex | Display::Grid
    );
    let has_block = children
        .iter()
        .any(|&child| tree.node(child).is_block_level());
    let has_inline = children
        .iter()
        .any(|&child| tree.node(child).is_inline_level());
    if !has_inline || !(has_block || flex) {
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

#[cfg(test)]
mod tests {
    use otlyra_css::cascade::{Viewport, style_document};

    use super::*;

    /// The box tree markup produces once its own stylesheets have been applied.
    fn styled(html: &str) -> BoxTree {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styles = style_document(&document, Viewport::default());
        build_box_tree(&document, &styles)
    }

    fn style_of(tree: &BoxTree, tag: &str) -> Arc<ComputedStyle> {
        tree.descendants(tree.root())
            .into_iter()
            .find(|&id| {
                tree.node(id)
                    .tag
                    .as_ref()
                    .is_some_and(|name| name.as_ref() == tag)
            })
            .map(|id| Arc::clone(&tree.node(id).style))
            .unwrap_or_else(|| panic!("no <{tag}> box"))
    }

    #[test]
    fn an_author_rule_changes_the_boxes() {
        let tree = styled("<style>p { color: #0f0; font-size: 30px }</style><p>text");
        let style = style_of(&tree, "p");
        assert_eq!(style.font_size, 30.0);
        let rgba = style.color.to_rgba8();
        assert_eq!([rgba.r, rgba.g, rgba.b], [0, 255, 0]);
    }

    /// An author rule can remove a box, which is the difference between "the
    /// cascade ran" and "the cascade is only consulted for colours".
    #[test]
    fn display_none_from_a_stylesheet_generates_no_box() {
        let tree = styled("<style>p { display: none }</style><p>text</p><div>kept</div>");
        let dump = crate::dump::serialize(&tree);
        assert!(!dump.contains("text"), "{dump}");
        assert!(dump.contains("kept"), "{dump}");
    }

    #[test]
    fn a_style_attribute_reaches_the_box_tree() {
        let tree = styled("<p style=\"font-size: 21px\">text");
        assert_eq!(style_of(&tree, "p").font_size, 21.0);
    }

    /// The text of a tree, run by run, which is what white-space processing is
    /// judged on: what the shaper is handed and nothing else.
    fn runs_of(html: &str) -> Vec<String> {
        let tree = styled(html);
        tree.descendants(tree.root())
            .into_iter()
            .filter_map(|id| match &tree.node(id).kind {
                BoxKind::Text(text) => Some(text.to_string()),
                _ => None,
            })
            .collect()
    }

    /// What the runs come to once they are joined, which is the line the reader
    /// sees.
    fn text_of(html: &str) -> String {
        runs_of(html).concat()
    }

    /// What a browser without plug-ins or frames would show, and a popover
    /// nothing has opened, leave no text behind — not even the markup inside a
    /// `noembed`, which is parsed as text.
    #[test]
    fn what_the_user_agent_sheet_hides_has_no_text() {
        for html in [
            "<noembed><b>x</b></noembed>",
            "<noframes>x</noframes>",
            "<div popover>x</div>",
        ] {
            assert!(runs_of(html).is_empty(), "{html}: {:?}", runs_of(html));
        }
        // An inline SVG is laid out as boxes, and its title and its style sheet
        // are not among them.
        assert_eq!(
            text_of("<p><svg><title>Logo</title><style>.a { fill: red }</style></svg>x"),
            "x"
        );
    }

    /// Collapsing is a fact about the formatting context, not about the text
    /// node: every case here is one the node on its own cannot answer.
    #[test]
    fn white_space_collapses_across_the_whole_context() {
        assert_eq!(
            text_of("<p><span>a </span><span>b</span>"),
            "a b",
            "a space ending one run is the space before the next"
        );
        assert_eq!(
            text_of("<p><span>a </span> <span>b</span>"),
            "a b",
            "and the space between the two elements is the same space"
        );
        assert_eq!(
            text_of("<p>   leading and trailing   "),
            "leading and trailing",
            "the ends of a context are not spaces"
        );
        assert_eq!(
            text_of("<p>a\nb"),
            "a b",
            "a line ending in the source is one more space"
        );
        assert_eq!(
            text_of("<p><span>x</span>\n<span>y</span>"),
            "x y",
            "including the one that indents the markup"
        );
        assert_eq!(
            text_of("<p>a<br> b"),
            "ab",
            "a space after a forced break is the start of a line, and goes"
        );
        assert_eq!(
            text_of("<p>a <br>b"),
            "ab",
            "and one in front of it is the end of one"
        );
    }

    /// The other three modes, which differ in what survives.
    #[test]
    fn preserved_white_space_is_kept_exactly() {
        assert_eq!(
            text_of("<p style=\"white-space: pre\">  two   spaces\nsecond"),
            "  two   spaces\nsecond",
            "`pre` keeps every one of them"
        );
        assert_eq!(
            text_of("<p style=\"white-space: pre-wrap\">  two   spaces\nsecond"),
            "  two   spaces\nsecond",
            "and so does `pre-wrap`"
        );
        assert_eq!(
            text_of("<p style=\"white-space: pre-line\">  two   spaces\nsecond"),
            "two spaces\nsecond",
            "`pre-line` keeps the break and collapses the rest"
        );
        assert_eq!(
            text_of("<p style=\"white-space: break-spaces\">  two   spaces"),
            "  two   spaces",
            "`break-spaces` keeps them and lets a line break inside them"
        );
    }

    /// A space beside something that is not text is not trailing white space:
    /// there is something after it.
    #[test]
    fn a_space_beside_a_picture_is_a_word_gap() {
        assert_eq!(
            text_of("<p>word <img src=x.png>"),
            "word ",
            "the space in front of a picture is the gap between them"
        );
        assert_eq!(
            text_of("<p><img src=x.png> word"),
            " word",
            "and so is the one after it"
        );
        assert_eq!(
            text_of("<p>\n  <img src=x.png>\n  <img src=x.png>\n"),
            " ",
            "but the markup around them is one gap and nothing at either end"
        );
    }

    /// The space between two blocks is not a word gap; the space between two
    /// controls is the only thing keeping them apart.
    #[test]
    fn whitespace_survives_between_inline_boxes_and_not_between_blocks() {
        let inline = crate::dump::serialize(&styled("<p><button>a</button> <button>b</button>"));
        assert!(
            inline.contains("TEXT \" \""),
            "the gap between two controls is gone:\n{inline}"
        );

        let blocks = crate::dump::serialize(&styled("<div>a</div>\n<div>b</div>"));
        assert!(
            !blocks.contains("TEXT \" \""),
            "a newline between two blocks became a line box:\n{blocks}"
        );
    }

    /// An element the cascade has not styled has nothing to be laid out with,
    /// so it makes no box — rather than one styled by rules no page can see.
    #[test]
    fn an_element_the_cascade_did_not_reach_makes_no_box() {
        let mut document = otlyra_html::parse(b"<p>styled", Some("utf-8")).document;
        let styles = style_document(&document, Viewport::default());
        let body = document
            .first_element_child(document.root())
            .and_then(|html| document.first_element_child(html))
            .and_then(|head| document.next_element_sibling(head))
            .expect("the parser made a body");
        let mut dom = otlyra_dom::DocumentMutator::new(&mut document);
        let late = dom.create_element(
            html5ever::QualName::new(None, html5ever::ns!(html), "h1".into()),
            Vec::new(),
            None,
            false,
        );
        dom.append(body, late);
        dom.append_text(late, "late".into());

        let dump = crate::dump::serialize(&build_box_tree(&document, &styles));
        assert!(dump.contains("styled"), "{dump}");
        assert!(!dump.contains("h1") && !dump.contains("late"), "{dump}");
    }

    /// What is inside a `<script>` or an `<object>` is not prose, but only
    /// HTML's elements of those names mean that: an SVG one is just an element.
    #[test]
    fn only_html_elements_hide_their_children() {
        assert!(
            !text_of("<p><video>fallback</video>after").contains("fallback"),
            "an HTML <video> keeps its fallback to itself"
        );
        assert!(
            text_of("<p><svg><video>drawn</video></svg>").contains("drawn"),
            "an SVG element that shares the name is an element like any other"
        );
    }

    /// What is inside a `video`, an `audio` or a `canvas` is for a browser that
    /// has none; what is inside an `object` is what it shows when its resource
    /// is not a picture (HTML §4.8.7), which without one arriving it is not.
    #[test]
    fn fallback_is_shown_only_by_an_object() {
        for html in [
            "<video>x</video>",
            "<audio controls>x</audio>",
            "<canvas>x</canvas>",
            "<iframe>x</iframe>",
        ] {
            assert_eq!(text_of(html), "", "{html}");
        }
        assert_eq!(text_of("<object data=x.swf>fallback</object>"), "fallback");
    }

    /// The pictures a page asks for: an `img`'s, a video's poster, an image
    /// button's, and whatever an `embed` or an `object` names, which may turn
    /// out to be one. An address of nothing but spaces is no address, and a
    /// submit button's `src` is no picture.
    #[test]
    fn every_element_that_shows_a_picture_asks_for_it() {
        let document = otlyra_html::parse(
            b"<img src=a.png><video poster=' p.png '></video><video poster=' '></video>\
              <input type=image src=b.png><input type=submit src=c.png>\
              <embed src=d.swf><embed><object data=e.svg></object><svg><video poster=f.png>",
            Some("utf-8"),
        )
        .document;
        let sources: Vec<String> = image_sources(&document, Viewport::default())
            .into_iter()
            .map(|source| source.src)
            .collect();
        assert_eq!(sources, ["a.png", "p.png", "b.png", "d.swf", "e.svg"]);
    }

    /// A video's poster is the picture its box shows, at the poster's size.
    #[test]
    fn a_poster_is_a_videos_picture() {
        let document = otlyra_html::parse(b"<video poster=p.png></video>", Some("utf-8")).document;
        let styles = style_document(&document, Viewport::default());
        let data = otlyra_gfx::peniko::ImageData {
            data: otlyra_gfx::peniko::Blob::new(Arc::new(vec![0; 8 * 6 * 4])),
            format: otlyra_gfx::peniko::ImageFormat::Rgba8,
            alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
            width: 8,
            height: 6,
        };
        let images: Images = image_sources(&document, Viewport::default())
            .into_iter()
            .map(|source| (source.node, Picture::new(data.clone())))
            .collect();
        let tree = build_box_tree_with_images(&document, &styles, &images);
        let video = tree
            .descendants(tree.root())
            .into_iter()
            .find(|&id| {
                tree.node(id)
                    .tag
                    .as_ref()
                    .is_some_and(|tag| tag.as_ref() == "video")
            })
            .expect("a video box");
        let BoxKind::Replaced(content) = &tree.node(video).kind else {
            panic!("a video is replaced");
        };
        assert_eq!(content.image.as_ref(), Some(&data));
        assert_eq!(content.intrinsic, Some((8.0, 6.0)));
    }
}
