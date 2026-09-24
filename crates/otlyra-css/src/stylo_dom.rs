//! Our DOM, seen through Stylo's eyes.
//!
//! The style engine does not own a DOM; it operates over one through traits, and
//! this file is our side of that boundary: a handle type that is `Copy`, and the
//! trait implementations that answer every question the style system asks about a
//! node.
//!
//! This is the first half — [`selectors::Element`], which is what matching needs.
//! It is useful on its own: `div.note > p:first-child` can be matched against a
//! parsed document before any cascade exists. The rest of `TElement` — element
//! data, restyle damage, the traversal — follows it.

use std::fmt;

use otlyra_dom::{Document, ElementData, NodeId};
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::matching::{ElementSelectorFlags, MatchingContext};
use selectors::{Element as SelectorsElement, OpaqueElement};
use style::selector_parser::{
    AttrValue, NonTSPseudoClass, PseudoElement, SelectorImpl as StyleSelectorImpl,
};
use style::values::AtomIdent;

/// The names Stylo hands the matcher.
///
/// Element names arrive as the plain interned atom html5ever produces; attribute
/// names arrive inside Stylo's `GenericAtomIdent` newtype around the same atom.
/// The two spellings are not interchangeable to the compiler, which is why both
/// are named here rather than guessed at each call site.
type BorrowedLocalName = <StyleSelectorImpl as selectors::SelectorImpl>::BorrowedLocalName;
type BorrowedNamespace = <StyleSelectorImpl as selectors::SelectorImpl>::BorrowedNamespaceUrl;
type LocalNameIdent = style::values::GenericAtomIdent<html5ever::LocalNameStaticSet>;
type NamespaceIdent = style::values::GenericAtomIdent<html5ever::NamespaceStaticSet>;

/// A document together with its style state.
///
/// The pair exists so that a node handle can be *two* words: one pointer here and
/// one node id. The style engine's sharing cache asserts the size of the handle it
/// was compiled for, and it is right to — the cache is an array of them, and a
/// handle that carries a third word makes every entry pay for it.
pub struct Tree<'a> {
    /// The document.
    pub document: &'a Document,
    /// The style engine's per-element state, beside the document rather than in
    /// it: the engine borrows it mutably while holding only a shared reference to
    /// the tree, which is a shape the DOM should not have to take.
    ///
    /// Absent for a tree used only to match selectors, which needs no state.
    pub style_data: Option<&'a StyleData>,
    /// Where the state bits come from when there is no per-element state to read
    /// them out of.
    ///
    /// A styled tree has them cached in each element's slot, computed once when
    /// the slots were made. A tree built only to match a selector has no slots, and
    /// this is what answers instead — the same computation, done per question.
    pub states: Option<&'a crate::state::States<'a>>,
    /// The document's pragma-set default language, worked out the first time a
    /// `:lang()` reaches an element nothing above has a language for, and not
    /// before: a restyle that never asks pays nothing for it.
    default_language: std::cell::OnceCell<Option<Box<str>>>,
}

impl<'a> Tree<'a> {
    /// A tree that can only be matched against.
    pub fn new(document: &'a Document) -> Self {
        Self {
            document,
            style_data: None,
            states: None,
            default_language: std::cell::OnceCell::new(),
        }
    }

    /// The same, with the state bits a selector may ask for.
    pub fn matching(document: &'a Document, states: &'a crate::state::States<'a>) -> Self {
        Self {
            document,
            style_data: None,
            states: Some(states),
            default_language: std::cell::OnceCell::new(),
        }
    }

    /// A tree that can be styled.
    pub fn styled(document: &'a Document, style_data: &'a StyleData) -> Self {
        Self {
            document,
            style_data: Some(style_data),
            states: None,
            default_language: std::cell::OnceCell::new(),
        }
    }

    /// The language of an element that neither it nor anything above it gives
    /// one (HTML, "The `lang` and `xml:lang` attributes"): the pragma-set default
    /// language, if a `<meta http-equiv=content-language>` set one.
    ///
    /// The language a response's `Content-Language` header names comes after it,
    /// and is not here: the header does not reach the cascade.
    fn default_language(&self) -> Option<&str> {
        self.default_language
            .get_or_init(|| pragma_set_default_language(self.document))
            .as_deref()
    }

    /// A handle to one node in it.
    ///
    /// Only meaningful inside a [`TreeScope`] for this tree, which is what the
    /// handle's lifetime is tied to.
    pub fn node(&'a self, id: NodeId) -> NodeRef<'a> {
        NodeRef {
            id,
            tree: std::marker::PhantomData,
        }
    }
}

/// What the document's `<meta http-equiv=content-language>` elements leave the
/// pragma-set default language at (HTML §4.2.5.3, "Content language state").
///
/// Each one sets it as it is inserted, so the last in tree order that says
/// anything wins, as it does in Chrome. One whose content lists more than one
/// language, or none, says nothing; one that says anything says its first word.
fn pragma_set_default_language(document: &Document) -> Option<Box<str>> {
    let mut language = None;
    let mut stack = vec![document.root()];
    while let Some(id) = stack.pop() {
        let children: Vec<NodeId> = document.children(id).collect();
        stack.extend(children.into_iter().rev());
        let Some(element) = document.get(id).and_then(|node| node.element()) else {
            continue;
        };
        let is_pragma = element.name.ns == html5ever::ns!(html)
            && element.name.local.as_ref() == "meta"
            && element
                .attr("http-equiv")
                .is_some_and(|state| state.eq_ignore_ascii_case("content-language"));
        if !is_pragma {
            continue;
        }
        let Some(content) = element
            .attr("content")
            .filter(|content| !content.contains(','))
        else {
            continue;
        };
        if let Some(candidate) = content.split_ascii_whitespace().next() {
            language = Some(Box::from(candidate));
        }
    }
    language
}

/// Whether a language tag falls in a language range (Selectors 4 §7.2, after
/// RFC 4647's extended filtering, without its wildcards): the range itself, or
/// the range and more subtags after a hyphen, compared ignoring ASCII case — so
/// `en` takes in `en-GB` and not `eng`.
fn language_range_matches(range: &str, tag: &str) -> bool {
    tag.get(..range.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(range))
        && matches!(tag.as_bytes().get(range.len()), None | Some(b'-'))
}

thread_local! {
    /// The tree the current restyle is walking.
    ///
    /// A node handle has to be **one word**: the style engine's sharing cache is an
    /// array of entries compiled for an element of pointer size, and it asserts
    /// that size at construction. Our nodes live in an arena, so a self-contained
    /// handle would be two — the arena and the index. The arena therefore moves out
    /// of the handle and into the scope, which is exactly as wide as one restyle.
    static CURRENT_TREE: std::cell::Cell<*const Tree<'static>> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

/// Makes a tree current for as long as it is held.
///
/// Restores the previous tree on drop rather than clearing, so that a nested scope
/// — a document styled while another is being styled — leaves the outer one intact.
pub struct TreeScope {
    previous: *const Tree<'static>,
}

impl TreeScope {
    /// Enter `tree`.
    ///
    /// The lifetime is erased on the way in and restored on the way out by the
    /// scope: nothing can observe a handle outside the scope, because every handle
    /// borrows from it.
    pub fn enter(tree: &Tree<'_>) -> Self {
        // SAFETY: the pointer is only read while this scope is alive, and the scope
        // borrows the tree for its whole lifetime. The erased lifetime is never
        // handed back out: `NodeRef::tree` reborrows for the caller's lifetime,
        // which cannot outlive the scope that produced the handle.
        let erased = unsafe {
            std::mem::transmute::<*const Tree<'_>, *const Tree<'static>>(std::ptr::from_ref(tree))
        };
        let previous = CURRENT_TREE.with(|current| current.replace(erased));
        Self { previous }
    }
}

impl Drop for TreeScope {
    fn drop(&mut self) {
        CURRENT_TREE.with(|current| current.set(self.previous));
    }
}

/// A borrowed handle to one node.
///
/// `Copy` and one word wide, both because the style engine requires it. The tree it
/// belongs to is the one currently in scope; see [`TreeScope`] for why it cannot be
/// carried here.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    /// Which node.
    pub id: NodeId,
    /// Ties the handle to the scope that produced it.
    tree: std::marker::PhantomData<&'a Tree<'a>>,
}

impl<'a> NodeRef<'a> {
    /// The tree this handle belongs to.
    fn tree(&self) -> &'a Tree<'a> {
        let pointer = CURRENT_TREE.with(std::cell::Cell::get);
        assert!(
            !pointer.is_null(),
            "a node handle used outside a tree scope"
        );
        // SAFETY: the handle's lifetime is tied to the scope that produced it, and
        // the scope keeps the tree borrowed and the pointer current for exactly as
        // long as it lives.
        unsafe { &*pointer }
    }

    /// The document this node lives in.
    pub fn document(&self) -> &'a Document {
        self.tree().document
    }

    /// This element's slot in the style state, if it has one.
    fn slot(&self) -> Option<&'a ElementSlot> {
        self.tree().style_data?.slots.get(&self.id)
    }

    /// Every state bit this element holds.
    ///
    /// Cached in the slot during a restyle, because matching asks for it once per
    /// selector that mentions a state and the answer cannot change mid-restyle.
    fn element_state(&self) -> stylo_dom::ElementState {
        if let Some(slot) = self.slot() {
            return slot.state;
        }
        self.tree()
            .states
            .map_or_else(stylo_dom::ElementState::empty, |states| {
                states.state_of(self.id)
            })
    }

    /// The element data, if this node is an element.
    pub fn element(&self) -> Option<&'a ElementData> {
        self.tree().document.get(self.id)?.element()
    }

    /// The same handle, pointed at another node.
    fn at(&self, id: NodeId) -> Self {
        Self {
            id,
            tree: std::marker::PhantomData,
        }
    }

    /// The nearest ancestor that is an element.
    fn parent_element_id(&self) -> Option<NodeId> {
        let parent = self.tree().document.get(self.id)?.parent?;
        self.tree().document.get(parent)?.element().map(|_| parent)
    }
}

impl fmt::Debug for NodeRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.element() {
            Some(element) => write!(f, "<{}>", element.name.local),
            None => write!(f, "#node"),
        }
    }
}

impl PartialEq for NodeRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        // Identity is the node, not the handle: two handles into the same document
        // and the same node are the same element, which is what selector matching
        // means by equality.
        self.id == other.id
    }
}

impl Eq for NodeRef<'_> {}

impl std::hash::Hash for NodeRef<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // The node, not the handle: equal handles must hash equally, and two
        // handles to the same node are equal.
        self.id.hash(state);
    }
}

impl style::dom::AttributeProvider for NodeRef<'_> {
    fn get_attr(&self, name: &LocalNameIdent, namespace: &NamespaceIdent) -> Option<String> {
        self.element()?
            .attrs
            .iter()
            .find(|attr| attr.name.local == name.0 && attr.name.ns == namespace.0)
            .map(|attr| attr.value.to_string())
    }
}

impl SelectorsElement for NodeRef<'_> {
    type Impl = StyleSelectorImpl;

    fn opaque(&self) -> OpaqueElement {
        // An identity the matcher can compare and hash without knowing what it is.
        // Derived from the node's own key rather than a pointer: our nodes live in
        // a slotmap and do not have stable addresses.
        OpaqueElement::new(&self.tree().document.node(self.id).data)
    }

    fn parent_element(&self) -> Option<Self> {
        self.parent_element_id().map(|id| self.at(id))
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.tree()
            .document
            .prev_element_sibling(self.id)
            .map(|id| self.at(id))
    }

    fn next_sibling_element(&self) -> Option<Self> {
        self.tree()
            .document
            .next_element_sibling(self.id)
            .map(|id| self.at(id))
    }

    fn first_element_child(&self) -> Option<Self> {
        self.tree()
            .document
            .first_element_child(self.id)
            .map(|id| self.at(id))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.element()
            .is_some_and(|element| element.name.ns == html5ever::ns!(html))
    }

    fn has_local_name(&self, name: &BorrowedLocalName) -> bool {
        self.element()
            .is_some_and(|element| &element.name.local == name)
    }

    fn has_namespace(&self, namespace: &BorrowedNamespace) -> bool {
        self.element()
            .is_some_and(|element| &element.name.ns == namespace)
    }

    fn is_same_type(&self, other: &Self) -> bool {
        match (self.element(), other.element()) {
            (Some(a), Some(b)) => a.name.local == b.name.local && a.name.ns == b.name.ns,
            _ => false,
        }
    }

    fn attr_matches(
        &self,
        namespace: &NamespaceConstraint<&NamespaceIdent>,
        local_name: &LocalNameIdent,
        operation: &AttrSelectorOperation<&AttrValue>,
    ) -> bool {
        let Some(element) = self.element() else {
            return false;
        };
        element.attrs.iter().any(|attr| {
            if attr.name.local != local_name.0 {
                return false;
            }
            match namespace {
                NamespaceConstraint::Any => {}
                NamespaceConstraint::Specific(ns) => {
                    if attr.name.ns != ns.0 {
                        return false;
                    }
                }
            }
            operation.eval_str(&attr.value)
        })
    }

    fn has_attr_in_no_namespace(&self, local_name: &LocalNameIdent) -> bool {
        self.element().is_some_and(|element| {
            element
                .attrs
                .iter()
                .any(|attr| attr.name.ns == html5ever::ns!() && attr.name.local == local_name.0)
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &NonTSPseudoClass,
        _context: &mut MatchingContext<'_, Self::Impl>,
    ) -> bool {
        // Nearly every one of these is a bit: the engine's own table says which,
        // and the state was computed for this element before matching began. The
        // few that are not — a language, a custom state — report an empty flag,
        // and an empty flag is contained by every state, so they have to be turned
        // away before the question is asked rather than after.
        let flag = pseudo_class.state_flag();
        if !flag.is_empty() {
            return self.element_state().intersects(flag);
        }
        match pseudo_class {
            NonTSPseudoClass::Lang(range) => {
                style::dom::TElement::match_element_lang(self, None, range)
            }
            // A custom state is set by the element's own script, through the
            // custom element API, which there is none of.
            NonTSPseudoClass::CustomState(_) => false,
            // Stylo's name for a table whose `border` attribute draws one, which
            // is a question about the attribute's parsed value rather than its
            // text.
            NonTSPseudoClass::ServoNonZeroBorder => {
                self.element().is_some_and(crate::hints::draws_border)
            }
            _ => false,
        }
    }

    fn match_pseudo_element(
        &self,
        _pseudo_element: &PseudoElement,
        _context: &mut MatchingContext<'_, Self::Impl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {
        // The flags tell a mutable DOM which elements must be restyled when their
        // siblings or children change. Ours is rebuilt wholesale on every load, so
        // there is nothing yet for them to save — and recording them would be state
        // that no invalidation reads.
    }

    fn is_link(&self) -> bool {
        self.element().is_some_and(|element| {
            matches!(element.name.local.as_ref(), "a" | "area" | "link")
                && element.attr("href").is_some()
        })
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn has_id(&self, id: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.element()
            .and_then(ElementData::id)
            .is_some_and(|value| case_sensitivity.eq(value.as_bytes(), id.as_bytes()))
    }

    fn has_class(&self, name: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.element().is_some_and(|element| {
            element
                .classes()
                .any(|class| case_sensitivity.eq(class.as_bytes(), name.as_bytes()))
        })
    }

    fn has_custom_state(&self, _name: &AtomIdent) -> bool {
        false
    }

    fn imported_part(&self, _name: &AtomIdent) -> Option<AtomIdent> {
        None
    }

    fn is_part(&self, _name: &AtomIdent) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.tree().document.is_empty_element(self.id)
    }

    fn is_root(&self) -> bool {
        // The root element, not the document node: `:root` is `<html>`.
        self.parent_element_id().is_none()
            && self
                .tree()
                .document
                .get(self.id)
                .and_then(|node| node.parent)
                .is_some_and(|parent| parent == self.tree().document.root())
    }

    fn add_element_unique_hashes(&self, filter: &mut BloomFilter) -> bool {
        // The ancestor filter: a quick "this subtree cannot match" test that saves
        // the matcher from walking up the tree for most rules. Feeding it the same
        // three things every engine does — name, id, classes — is what makes it
        // effective rather than merely present.
        let Some(element) = self.element() else {
            return false;
        };

        let mut hash = |value: &str| {
            filter.insert_hash(fxhash(value));
        };
        hash(element.name.local.as_ref());
        if let Some(id) = element.id() {
            hash(id);
        }
        for class in element.classes() {
            hash(class);
        }
        true
    }
}

/// The hash the bloom filter is fed.
///
/// Any stable hash will do — the filter only ever asks "have I seen this" — but it
/// must be the *same* one on both sides, which is why it lives here rather than at
/// each call site.
fn fxhash(value: &str) -> u32 {
    let mut hash: u32 = 0;
    for byte in value.as_bytes() {
        hash = hash.rotate_left(5) ^ u32::from(*byte);
        hash = hash.wrapping_mul(0x9E37_79B9);
    }
    hash
}

/// Match `selector` against every element in `document`, in tree order.
///
/// The matching engine over our tree — the thing the cascade will use for every
/// rule, exercised here on its own so that "does this selector match this element"
/// is answerable and testable before any cascade exists.
pub fn select(document: &Document, selector: &str) -> Result<Vec<NodeId>, String> {
    let form = otlyra_dom::FormState::new();
    select_with(document, selector, &form, crate::state::Interaction::none())
}

/// The same, against controls that hold something and a pointer that is somewhere.
///
/// `:checked` and `:hover` are only answerable with those two, and both are
/// arguments rather than document state for the reason given in [`crate::state`].
pub fn select_with(
    document: &Document,
    selector: &str,
    form: &otlyra_dom::FormState,
    interaction: crate::state::Interaction,
) -> Result<Vec<NodeId>, String> {
    use selectors::matching::{
        MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags,
        QuirksMode as SelectorsQuirksMode,
    };
    use style::selector_parser::SelectorParser;
    use style::stylesheets::UrlExtraData;

    // Every stylesheet and every selector list is parsed against a base URL, which
    // is what `url()` inside it would resolve against. A selector has none, so this
    // is a stand-in that cannot resolve to anything.
    let url = UrlExtraData(servo_arc::Arc::new(crate::cascade::about_blank()));
    let list = SelectorParser::parse_author_origin_no_namespace(selector, &url)
        .map_err(|error| format!("bad selector {selector:?}: {error:?}"))?;

    let mut caches = selectors::context::SelectorCaches::default();
    let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        SelectorsQuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );

    let states = crate::state::States::new(document, form, interaction);
    let tree = Tree::matching(document, &states);
    let _scope = TreeScope::enter(&tree);
    let mut matched = Vec::new();
    let mut stack = vec![document.root()];
    let mut order = Vec::new();
    while let Some(id) = stack.pop() {
        order.push(id);
        let children: Vec<NodeId> = document.children(id).collect();
        stack.extend(children.into_iter().rev());
    }

    for id in order {
        let node = tree.node(id);
        if node.element().is_none() {
            continue;
        }
        if list.slice().iter().any(|selector| {
            selectors::matching::matches_selector(selector, 0, None, &node, &mut context)
        }) {
            matched.push(id);
        }
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids of the elements a selector matched, given what the controls hold
    /// and where the pointer is.
    fn matching_state(
        html: &str,
        selector: &str,
        interaction: crate::state::Interaction,
    ) -> Vec<String> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let form = otlyra_dom::FormState::new();
        select_with(&document, selector, &form, interaction)
            .expect("the selector should parse")
            .into_iter()
            .map(|id| {
                let element = document.node(id).element().expect("an element");
                element
                    .id()
                    .map_or_else(|| element.name.local.to_string(), str::to_owned)
            })
            .collect()
    }

    /// The node whose `id` attribute is `id`.
    fn node_with_id(document: &otlyra_dom::Document, id: &str) -> NodeId {
        let mut stack = vec![document.root()];
        while let Some(node) = stack.pop() {
            if document
                .get(node)
                .and_then(|node| node.element())
                .and_then(|element| element.id())
                == Some(id)
            {
                return node;
            }
            stack.extend(document.children(node));
        }
        panic!("no element with id {id}");
    }

    #[test]
    fn a_checkbox_is_checked_by_its_attribute_and_a_plain_one_is_not() {
        assert_eq!(
            matching_state(
                "<input id=on type=checkbox checked><input id=off type=checkbox>",
                ":checked",
                crate::state::Interaction::none(),
            ),
            ["on"]
        );
    }

    #[test]
    fn a_selected_option_is_checked_and_so_is_the_first_one_of_a_drop_down() {
        assert_eq!(
            matching_state(
                "<select><option id=a>A<option id=b selected>B</select>",
                "option:checked",
                crate::state::Interaction::none(),
            ),
            ["b"]
        );
        // A drop-down always shows something, so its first option is selected even
        // though nothing in the markup says so.
        assert_eq!(
            matching_state(
                "<select><option id=a>A<option id=b>B</select>",
                "option:checked",
                crate::state::Interaction::none(),
            ),
            ["a"]
        );
        // A list box shows no such thing.
        assert_eq!(
            matching_state(
                "<select multiple><option id=a>A<option id=b>B</select>",
                "option:checked",
                crate::state::Interaction::none(),
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_fieldset_disables_what_it_holds_but_not_its_first_legend() {
        assert_eq!(
            matching_state(
                "<fieldset disabled><legend><input id=in-legend></legend>\
                 <input id=inside></fieldset><input id=outside>",
                "input:disabled",
                crate::state::Interaction::none(),
            ),
            ["inside"]
        );
        assert_eq!(
            matching_state(
                "<fieldset disabled><legend><input id=in-legend></legend>\
                 <input id=inside></fieldset><input id=outside>",
                "input:enabled",
                crate::state::Interaction::none(),
            ),
            ["in-legend", "outside"]
        );
    }

    #[test]
    fn only_a_control_is_enabled_or_disabled() {
        assert_eq!(
            matching_state(
                "<div id=d></div><input id=i>",
                ":enabled",
                crate::state::Interaction::none(),
            ),
            ["i"]
        );
    }

    #[test]
    fn everything_that_is_not_a_live_text_control_is_read_only() {
        assert_eq!(
            matching_state(
                "<div id=d></div><input id=field><input id=locked readonly>\
                 <input id=box type=checkbox><div id=host contenteditable></div>",
                ":read-write",
                crate::state::Interaction::none(),
            ),
            ["field", "host"]
        );
        assert_eq!(
            matching_state(
                "<input id=field><input id=locked readonly>",
                "input:read-only",
                crate::state::Interaction::none(),
            ),
            ["locked"]
        );
    }

    #[test]
    fn required_is_only_answered_where_it_applies() {
        assert_eq!(
            matching_state(
                "<input id=need required><input id=free><div id=d required></div>",
                ":required",
                crate::state::Interaction::none(),
            ),
            ["need"]
        );
        assert_eq!(
            matching_state(
                "<input id=need required><input id=free><div id=d required></div>",
                ":optional",
                crate::state::Interaction::none(),
            ),
            ["free"]
        );
    }

    #[test]
    fn a_placeholder_shows_only_while_there_is_nothing_to_show_instead() {
        assert_eq!(
            matching_state(
                "<input id=empty placeholder=hint><input id=full placeholder=hint value=x>\
                 <input id=bare>",
                ":placeholder-shown",
                crate::state::Interaction::none(),
            ),
            ["empty"]
        );
    }

    #[test]
    fn a_radio_group_with_nothing_checked_is_indeterminate() {
        assert_eq!(
            matching_state(
                "<input id=a type=radio name=g><input id=b type=radio name=g>",
                ":indeterminate",
                crate::state::Interaction::none(),
            ),
            ["a", "b"]
        );
        assert_eq!(
            matching_state(
                "<input id=a type=radio name=g checked><input id=b type=radio name=g>",
                ":indeterminate",
                crate::state::Interaction::none(),
            ),
            Vec::<String>::new()
        );
        // A different name is a different group, and answers for itself.
        assert_eq!(
            matching_state(
                "<input id=a type=radio name=g checked><input id=b type=radio name=h>",
                ":indeterminate",
                crate::state::Interaction::none(),
            ),
            ["b"]
        );
    }

    #[test]
    fn hover_reaches_the_ancestors_of_what_the_pointer_is_over() {
        let html = "<div id=outer><p id=inner><a id=link href=#>x</a></p></div><p id=other>y</p>";
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let form = otlyra_dom::FormState::new();
        let interaction = crate::state::Interaction {
            hover: Some(node_with_id(&document, "link")),
            ..crate::state::Interaction::none()
        };
        let matched: Vec<String> = select_with(&document, ":hover", &form, interaction)
            .expect("the selector should parse")
            .into_iter()
            .filter_map(|id| {
                document
                    .node(id)
                    .element()
                    .and_then(|element| element.id())
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(matched, ["outer", "inner", "link"]);
    }

    #[test]
    fn focus_is_one_element_and_focus_within_is_the_rest_of_the_chain() {
        let html = "<form id=f><p id=p><input id=field></p></form>";
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let form = otlyra_dom::FormState::new();
        let field = node_with_id(&document, "field");
        let interaction = crate::state::Interaction {
            focus: Some(field),
            focus_visible: true,
            ..crate::state::Interaction::none()
        };
        let ids = |selector: &str| -> Vec<String> {
            select_with(&document, selector, &form, interaction)
                .expect("the selector should parse")
                .into_iter()
                .filter_map(|id| {
                    document
                        .node(id)
                        .element()
                        .and_then(|element| element.id())
                        .map(str::to_owned)
                })
                .collect()
        };
        assert_eq!(ids(":focus"), ["field"]);
        assert_eq!(ids(":focus-visible"), ["field"]);
        assert_eq!(ids(":focus-within"), ["f", "p", "field"]);
    }

    #[test]
    fn a_focus_ring_is_shown_only_when_the_reader_asked_for_one() {
        let html = "<input id=field>";
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let form = otlyra_dom::FormState::new();
        let interaction = crate::state::Interaction {
            focus: Some(node_with_id(&document, "field")),
            focus_visible: false,
            ..crate::state::Interaction::none()
        };
        assert_eq!(
            select_with(&document, ":focus", &form, interaction)
                .expect("the selector should parse")
                .len(),
            1
        );
        assert!(
            select_with(&document, ":focus-visible", &form, interaction)
                .expect("the selector should parse")
                .is_empty()
        );
    }

    #[test]
    fn what_the_reader_did_outranks_what_the_markup_says() {
        let html = "<input id=a type=checkbox checked><input id=b type=checkbox>";
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let mut form = otlyra_dom::FormState::new();
        form.set_checkedness(node_with_id(&document, "a"), false);
        form.set_checkedness(node_with_id(&document, "b"), true);
        let matched: Vec<String> = select_with(
            &document,
            ":checked",
            &form,
            crate::state::Interaction::none(),
        )
        .expect("the selector should parse")
        .into_iter()
        .filter_map(|id| {
            document
                .node(id)
                .element()
                .and_then(|element| element.id())
                .map(str::to_owned)
        })
        .collect();
        assert_eq!(matched, ["b"]);
        // `:default` still names what the markup nominated.
        assert_eq!(
            matching_state(html, ":default", crate::state::Interaction::none()),
            ["a"]
        );
    }

    #[test]
    fn details_is_open_by_its_attribute() {
        assert_eq!(
            matching_state(
                "<details id=a open><summary>s</summary>x</details><details id=b>y</details>",
                ":open",
                crate::state::Interaction::none(),
            ),
            ["a"]
        );
    }

    /// The tags of the elements a selector matched, in tree order.
    fn matching(html: &str, selector: &str) -> Vec<String> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        select(&document, selector)
            .expect("the selector should parse")
            .into_iter()
            .map(|id| {
                let element = document.node(id).element().expect("an element");
                let mut label = element.name.local.to_string();
                if let Some(id) = element.id() {
                    label.push('#');
                    label.push_str(id);
                }
                label
            })
            .collect()
    }

    #[test]
    fn a_type_selector_matches_by_name() {
        assert_eq!(
            matching("<body><p>one</p><div>two</div><p>three", "p"),
            ["p", "p"]
        );
    }

    #[test]
    fn class_and_id_selectors_match_their_attributes() {
        let html = "<body><p class='note important'>a<p id=first class=note>b<p>c";
        assert_eq!(matching(html, ".note").len(), 2);
        assert_eq!(matching(html, ".note.important").len(), 1);
        assert_eq!(matching(html, "#first"), ["p#first"]);
        assert_eq!(matching(html, "p.note#first"), ["p#first"]);
    }

    /// The combinators, which are where a hand-written matcher goes wrong first:
    /// descendant walks every ancestor, child only the parent, `+` the element
    /// before and `~` any element before.
    #[test]
    fn combinators_walk_the_tree_the_way_css_says() {
        let html = "<body><div id=outer><section><p id=deep>x</p></section>                    <p id=child>y</p></div><p id=after>z</p>";

        assert_eq!(matching(html, "div p").len(), 2, "descendant");
        assert_eq!(matching(html, "div > p"), ["p#child"], "child");
        assert_eq!(matching(html, "section + p"), ["p#child"], "adjacent");
        assert_eq!(matching(html, "section ~ p"), ["p#child"], "sibling");
        assert_eq!(matching(html, "div + p"), ["p#after"]);
    }

    #[test]
    fn attribute_selectors_match_presence_and_value() {
        let html = "<body><a href='https://example.com/x'>a</a><a>b</a>                    <input type=text><input type=checkbox>";
        assert_eq!(matching(html, "a[href]").len(), 1);
        assert_eq!(matching(html, "[type=checkbox]").len(), 1);
        assert_eq!(matching(html, "[href^='https://']").len(), 1);
        assert_eq!(matching(html, "[href$='/x']").len(), 1);
        assert_eq!(matching(html, "[href*=example]").len(), 1);
    }

    #[test]
    fn structural_pseudo_classes_count_elements_not_nodes() {
        let html = "<ul>\n  <li>one</li>\n  <li>two</li>\n  <li>three</li>\n</ul>";
        assert_eq!(matching(html, "li:first-child").len(), 1);
        assert_eq!(matching(html, "li:last-child").len(), 1);
        assert_eq!(matching(html, "li:nth-child(2)").len(), 1);
        assert_eq!(
            matching(html, "li:nth-child(odd)").len(),
            2,
            "the whitespace between items must not be counted"
        );
    }

    #[test]
    fn empty_and_root_mean_what_the_spec_says() {
        assert_eq!(matching("<body><p></p><p>text", "p:empty").len(), 1);
        assert_eq!(matching("<body><p>x", ":root"), ["html"]);
    }

    /// `:is()`, `:where()` and `:not()` — the ones a browser written in 2020 has
    /// and one written in 2005 does not. They come with the selector parser we
    /// depend on rather than being ours to write.
    #[test]
    fn logical_combinations_are_supported() {
        let html = "<body><h1>a</h1><h2>b</h2><p>c</p>";
        assert_eq!(matching(html, ":is(h1, h2)").len(), 2);
        assert_eq!(matching(html, ":where(h1, h2)").len(), 2);
        assert_eq!(matching(html, "body > :not(p)").len(), 2);
    }

    #[test]
    fn a_link_is_a_link_only_with_an_href() {
        let html = "<body><a href=/x>yes</a><a>no</a>";
        assert_eq!(matching(html, "a:any-link").len(), 1);
        assert_eq!(matching(html, ":link").len(), 1);
    }

    /// `:lang()` asks for the nearest language declared up the tree, and takes a
    /// range: `en` is `en-FI` too, and not `eng`. An empty declaration is an
    /// unknown language, which no range matches.
    #[test]
    fn lang_is_the_nearest_declared_language_taken_as_a_range() {
        let html = "<html lang=en-FI><body><p id=a>x\
                    <div lang=''><p id=b>y</div><div lang=de><p id=c>z</div>";
        assert_eq!(matching(html, "html:lang(en)"), ["html"]);
        assert_eq!(matching(html, "p:lang(en)"), ["p#a"]);
        assert_eq!(matching(html, "p:lang(EN-fi)"), ["p#a"]);
        assert!(matching(html, "p:lang(e)").is_empty());
        assert!(matching(html, "p:lang(en-GB)").is_empty());
        assert_eq!(matching(html, "p:lang(de)"), ["p#c"]);
        assert!(matching(html, "p:lang('')").is_empty());
    }

    /// `xml:lang` in the XML namespace outranks `lang`; written on an HTML
    /// element it is in no namespace at all, and means nothing.
    #[test]
    fn xml_lang_outranks_lang() {
        let html = "<html lang=en><body><p id=p xml:lang=de>x\
                    <svg lang=en xml:lang=de><circle id=c r=1 /></svg>";
        assert_eq!(matching(html, "circle:lang(de)"), ["circle#c"]);
        assert!(matching(html, "circle:lang(en)").is_empty());
        assert_eq!(matching(html, "p:lang(en)"), ["p#p"]);
    }

    /// With no language declared anywhere above, the document's own default is
    /// the one its last `<meta http-equiv=content-language>` set — its first word,
    /// and only if it names a single language.
    #[test]
    fn a_content_language_pragma_is_the_default_language() {
        let pragma = |content: &str| {
            format!(
                "<meta http-equiv=content-language content='de'>\
                     <meta http-equiv=CONTENT-LANGUAGE content='{content}'><p id=a>x"
            )
        };
        assert_eq!(matching(&pragma("fr ca"), "p:lang(fr)"), ["p#a"]);
        assert_eq!(matching(&pragma("fr, ca"), "p:lang(de)"), ["p#a"]);
        assert_eq!(matching(&pragma(""), "p:lang(de)"), ["p#a"]);
        assert!(
            matching(
                "<meta http-equiv=content-language content=de><p lang=en id=a>x",
                "p:lang(de)"
            )
            .is_empty()
        );
        assert!(matching("<p id=a>x", "p:lang(en)").is_empty());
    }

    #[test]
    fn a_selector_that_does_not_parse_is_an_error_rather_than_a_panic() {
        let document = otlyra_html::parse(b"<p>x", Some("utf-8")).document;
        assert!(select(&document, "p >>> q").is_err());
        assert!(select(&document, "").is_err());
    }
}

/// One element's state, as the style engine wants it kept.
struct ElementSlot {
    /// The element's `id`, interned in the style engine's atom table rather than
    /// the DOM's: the two use different tables, and the engine compares by
    /// identity within its own.
    id: Option<style::Atom>,
    /// Its classes, likewise.
    classes: Vec<AtomIdent>,
    /// The names of its attributes, for the invalidation machinery.
    attr_names: Vec<style::LocalName>,
    /// Its `style=` attribute, parsed once.
    style_attribute: Option<
        servo_arc::Arc<style::shared_lock::Locked<style::properties::PropertyDeclarationBlock>>,
    >,
    /// What its presentational attributes stand for, worked out once. Behind the
    /// same lock as every other block the engine reads, and handed out as the same
    /// block every time the engine asks.
    hints: Option<
        servo_arc::Arc<style::shared_lock::Locked<style::properties::PropertyDeclarationBlock>>,
    >,
    /// The computed styles and restyle bookkeeping, in the wrapper the engine
    /// hands its own references out of.
    data: style::data::ElementDataWrapper,
    /// Whether a descendant needs restyling. The engine sets and clears this
    /// through `&self`, which is why it is a cell.
    dirty_descendants: std::sync::atomic::AtomicBool,
    /// The traversal's per-element counter, for the same reason.
    children_to_process: std::sync::atomic::AtomicIsize,
    /// Selector flags the matcher asked us to remember.
    selector_flags: std::sync::atomic::AtomicUsize,
    /// Every state bit the element holds: what the document says about it, and
    /// where the pointer and the focus are. Computed once per restyle, because a
    /// selector may ask for it many times and the answer cannot change while the
    /// restyle runs.
    state: stylo_dom::ElementState,
}

impl Default for ElementSlot {
    fn default() -> Self {
        Self {
            id: None,
            classes: Vec::new(),
            attr_names: Vec::new(),
            style_attribute: None,
            hints: None,
            data: style::data::ElementDataWrapper::default(),
            dirty_descendants: std::sync::atomic::AtomicBool::default(),
            children_to_process: std::sync::atomic::AtomicIsize::default(),
            selector_flags: std::sync::atomic::AtomicUsize::default(),
            state: stylo_dom::ElementState::empty(),
        }
    }
}

/// Where the style engine keeps its per-element state.
///
/// The engine wants a slot per element that it can borrow mutably while holding
/// only a shared reference to the tree — its restyle machinery is built that way.
/// Ours therefore lives beside the document: allocated in one pass before a
/// restyle, and read through interior mutability during it.
pub struct StyleData {
    slots: std::collections::HashMap<NodeId, ElementSlot>,
    /// The lock every stylesheet and declaration block in this document shares.
    lock: style::shared_lock::SharedRwLock,
}

impl Default for StyleData {
    fn default() -> Self {
        Self {
            slots: std::collections::HashMap::new(),
            lock: style::shared_lock::SharedRwLock::new(),
        }
    }
}

impl StyleData {
    /// Per-element state sharing an existing lock.
    ///
    /// The lock has to be the one the stylesheets were parsed under: a declaration
    /// block read through a different lock is a panic, not a wrong answer.
    pub fn with_lock(lock: style::shared_lock::SharedRwLock) -> Self {
        Self {
            slots: std::collections::HashMap::new(),
            lock,
        }
    }

    /// Make a slot for every element in `document`, and fill it.
    ///
    /// The matcher asks for an element's id, classes and attribute names through
    /// the style engine's own atom table, which is not the one the HTML parser
    /// interned them into — the two compare by identity within themselves and not
    /// with each other. Interning once here is what makes matching a pointer
    /// comparison rather than a string one, thousands of times per page.
    ///
    /// `base` is the document's base URL, which an address in a `style`
    /// attribute or a presentational hint resolves against.
    pub fn prepare(
        &mut self,
        document: &Document,
        form: &otlyra_dom::FormState,
        interaction: crate::state::Interaction,
        base: &style::stylesheets::UrlExtraData,
    ) {
        let mut every = Vec::new();
        let mut stack = vec![document.root()];
        while let Some(id) = stack.pop() {
            every.push(id);
            stack.extend(document.children(id));
        }
        self.prepare_nodes(document, &every, form, interaction, base);
    }

    /// The same, for a chosen few.
    ///
    /// Asking whether a state change is worth a restyle needs the matcher's view
    /// of a handful of elements — the ones the pointer and the focus touched — and
    /// nothing else. Building the whole document's slots to answer that would cost
    /// what the restyle costs, which is the cost being avoided.
    pub fn prepare_nodes(
        &mut self,
        document: &Document,
        nodes: &[NodeId],
        form: &otlyra_dom::FormState,
        interaction: crate::state::Interaction,
        base: &style::stylesheets::UrlExtraData,
    ) {
        let states = crate::state::States::new(document, form, interaction);
        let quirks_mode = match document.quirks_mode() {
            html5ever::interface::QuirksMode::Quirks => style::context::QuirksMode::Quirks,
            html5ever::interface::QuirksMode::LimitedQuirks => {
                style::context::QuirksMode::LimitedQuirks
            }
            html5ever::interface::QuirksMode::NoQuirks => style::context::QuirksMode::NoQuirks,
        };

        for &id in nodes {
            if let Some(element) = document.get(id).and_then(|node| node.element()) {
                let style_attribute = element.attr("style").and_then(|source| {
                    if source.trim().is_empty() {
                        return None;
                    }
                    let source = crate::appearance::rewrite_declarations(source);
                    let block = style::properties::parse_style_attribute(
                        &source,
                        base,
                        None,
                        quirks_mode,
                        style::stylesheets::CssRuleType::Style,
                    );
                    Some(servo_arc::Arc::new(self.lock.wrap(block)))
                });

                self.slots.insert(
                    id,
                    ElementSlot {
                        id: element.id().map(style::Atom::from),
                        classes: element.classes().map(AtomIdent::from).collect(),
                        attr_names: element
                            .attrs
                            .iter()
                            .map(|attr| style::LocalName::from(attr.name.local.as_ref()))
                            .collect(),
                        style_attribute,
                        hints: crate::hints::presentational_hints(document, id, base)
                            .map(|block| servo_arc::Arc::new(self.lock.wrap(block))),
                        state: states.state_of(id),
                        ..ElementSlot::default()
                    },
                );
            }
        }
    }

    /// Every state bit an element held when the styles were computed.
    ///
    /// Read back rather than recomputed: it was worked out once for the matcher,
    /// and what paints a control needs the same answer the cascade used. Two
    /// answers to "is this checked" is how a tick ends up in a box that no rule
    /// styled as checked.
    pub fn state_of(&self, node: NodeId) -> stylo_dom::ElementState {
        self.slots
            .get(&node)
            .map_or_else(stylo_dom::ElementState::empty, |slot| slot.state)
    }

    /// The lock every stylesheet and declaration block in this document shares.
    pub fn lock(&self) -> &style::shared_lock::SharedRwLock {
        &self.lock
    }

    /// Forget everything, as a new document does.
    pub fn clear(&mut self) {
        self.slots.clear();
    }
}

impl style::dom::NodeInfo for NodeRef<'_> {
    fn is_element(&self) -> bool {
        self.element().is_some()
    }

    fn is_text_node(&self) -> bool {
        matches!(
            self.tree().document.get(self.id).map(|node| &node.data),
            Some(otlyra_dom::NodeData::Text(_))
        )
    }
}

/// The document, as the style engine sees it.
#[derive(Clone, Copy)]
pub struct DocumentRef<'a> {
    node: NodeRef<'a>,
}

impl<'a> style::dom::TDocument for DocumentRef<'a> {
    type ConcreteNode = NodeRef<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        unreachable!("the document node is reached through the tree, not from here")
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> style::context::QuirksMode {
        match self.node.document().quirks_mode() {
            html5ever::interface::QuirksMode::NoQuirks => style::context::QuirksMode::NoQuirks,
            html5ever::interface::QuirksMode::LimitedQuirks => {
                style::context::QuirksMode::LimitedQuirks
            }
            html5ever::interface::QuirksMode::Quirks => style::context::QuirksMode::Quirks,
        }
    }

    fn shared_lock(&self) -> &style::shared_lock::SharedRwLock {
        self.node
            .tree()
            .style_data
            .expect("a document handle always carries style state")
            .lock()
    }
}

/// A shadow root, which we do not have.
///
/// The trait has to be satisfied for the types to line up; every method is
/// unreachable because no value of this type is ever constructed. Shadow DOM is a
/// milestone of its own, and a stub that lies would be worse than one that cannot
/// be called.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NoShadowRoot<'a> {
    /// Uninhabited, and carrying the tree's lifetime so that the associated types
    /// line up with the node type it would have belonged to.
    #[doc(hidden)]
    Never(std::convert::Infallible, std::marker::PhantomData<&'a ()>),
}

impl<'a> style::dom::TShadowRoot for NoShadowRoot<'a> {
    type ConcreteNode = NodeRef<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        match *self {
            NoShadowRoot::Never(never, _) => match never {},
        }
    }

    fn host(&self) -> <Self::ConcreteNode as style::dom::TNode>::ConcreteElement {
        match *self {
            NoShadowRoot::Never(never, _) => match never {},
        }
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        match *self {
            NoShadowRoot::Never(never, _) => match never {},
        }
    }
}

impl<'a> style::dom::TNode for NodeRef<'a> {
    type ConcreteElement = Self;
    type ConcreteDocument = DocumentRef<'a>;
    type ConcreteShadowRoot = NoShadowRoot<'a>;

    fn parent_node(&self) -> Option<Self> {
        self.tree()
            .document
            .get(self.id)?
            .parent
            .map(|id| self.at(id))
    }

    fn first_child(&self) -> Option<Self> {
        self.tree()
            .document
            .get(self.id)?
            .first_child()
            .map(|id| self.at(id))
    }

    fn last_child(&self) -> Option<Self> {
        self.tree()
            .document
            .get(self.id)?
            .last_child()
            .map(|id| self.at(id))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.tree()
            .document
            .get(self.id)?
            .prev_sibling()
            .map(|id| self.at(id))
    }

    fn next_sibling(&self) -> Option<Self> {
        self.tree()
            .document
            .get(self.id)?
            .next_sibling()
            .map(|id| self.at(id))
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        unreachable!("the document is reached through the tree, not from a node")
    }

    fn is_in_document(&self) -> bool {
        self.tree().document.get(self.id).is_some()
    }

    fn traversal_parent(&self) -> Option<Self> {
        self.parent_element_id().map(|id| self.at(id))
    }

    fn opaque(&self) -> style::dom::OpaqueNode {
        style::dom::OpaqueNode(otlyra_dom::node_id_to_u64(self.id) as usize)
    }

    fn debug_id(self) -> usize {
        otlyra_dom::node_id_to_u64(self.id) as usize
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        self.element().map(|_| *self)
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        None
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        None
    }
}

impl<'a> style::dom::TElement for NodeRef<'a> {
    type ConcreteNode = Self;
    type TraversalChildrenIterator = std::vec::IntoIter<Self>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn traversal_children(&self) -> style::dom::LayoutIterator<Self::TraversalChildrenIterator> {
        let children: Vec<Self> = self
            .tree()
            .document
            .children(self.id)
            .map(|id| self.at(id))
            .collect();
        style::dom::LayoutIterator(children.into_iter())
    }

    fn is_html_element(&self) -> bool {
        self.element()
            .is_some_and(|element| element.name.ns == html5ever::ns!(html))
    }

    fn is_mathml_element(&self) -> bool {
        self.element()
            .is_some_and(|element| element.name.ns.as_ref() == "http://www.w3.org/1998/Math/MathML")
    }

    fn is_svg_element(&self) -> bool {
        self.element()
            .is_some_and(|element| element.name.ns == html5ever::ns!(svg))
    }

    fn style_attribute(
        &self,
    ) -> Option<
        servo_arc::ArcBorrow<
            '_,
            style::shared_lock::Locked<style::properties::PropertyDeclarationBlock>,
        >,
    > {
        self.slot()?
            .style_attribute
            .as_ref()
            .map(servo_arc::Arc::borrow_arc)
    }

    fn animation_rule(
        &self,
        _context: &style::context::SharedStyleContext<'_>,
    ) -> Option<
        servo_arc::Arc<style::shared_lock::Locked<style::properties::PropertyDeclarationBlock>>,
    > {
        None
    }

    fn transition_rule(
        &self,
        _context: &style::context::SharedStyleContext<'_>,
    ) -> Option<
        servo_arc::Arc<style::shared_lock::Locked<style::properties::PropertyDeclarationBlock>>,
    > {
        None
    }

    fn state(&self) -> stylo_dom::ElementState {
        self.element_state()
    }

    fn has_part_attr(&self) -> bool {
        false
    }

    fn exports_any_part(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&style::Atom> {
        self.slot()?.id.as_ref()
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        let Some(slot) = self.slot() else { return };
        for class in &slot.classes {
            callback(class);
        }
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        let Some(slot) = self.slot() else { return };
        for name in &slot.attr_names {
            callback(name);
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        self.slot().is_some_and(|slot| {
            slot.dirty_descendants
                .load(std::sync::atomic::Ordering::Relaxed)
        })
    }

    fn has_snapshot(&self) -> bool {
        false
    }

    fn handled_snapshot(&self) -> bool {
        false
    }

    unsafe fn set_handled_snapshot(&self) {}

    unsafe fn set_dirty_descendants(&self) {
        if let Some(slot) = self.slot() {
            slot.dirty_descendants
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    unsafe fn unset_dirty_descendants(&self) {
        if let Some(slot) = self.slot() {
            slot.dirty_descendants
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn store_children_to_process(&self, n: isize) {
        if let Some(slot) = self.slot() {
            slot.children_to_process
                .store(n, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn did_process_child(&self) -> isize {
        self.slot().map_or(0, |slot| {
            slot.children_to_process
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
                - 1
        })
    }

    unsafe fn ensure_data(&self) -> style::data::ElementDataMut<'_> {
        self.slot()
            .expect("every element gets a slot before a restyle")
            .data
            .borrow_mut()
    }

    unsafe fn clear_data(&self) {
        if let Some(slot) = self.slot() {
            *slot.data.borrow_mut() = style::data::ElementData::default();
        }
    }

    fn has_data(&self) -> bool {
        self.slot().is_some()
    }

    fn borrow_data(&self) -> Option<style::data::ElementDataRef<'_>> {
        Some(self.slot()?.data.borrow())
    }

    fn mutate_data(&self) -> Option<style::data::ElementDataMut<'_>> {
        Some(self.slot()?.data.borrow_mut())
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        false
    }

    fn has_animations(&self, _context: &style::context::SharedStyleContext<'_>) -> bool {
        false
    }

    fn has_css_animations(
        &self,
        _context: &style::context::SharedStyleContext<'_>,
        _pseudo: Option<style::selector_parser::PseudoElement>,
    ) -> bool {
        false
    }

    fn has_css_transitions(
        &self,
        _context: &style::context::SharedStyleContext<'_>,
        _pseudo: Option<style::selector_parser::PseudoElement>,
    ) -> bool {
        false
    }

    fn shadow_root(&self) -> Option<NoShadowRoot<'a>> {
        None
    }

    fn containing_shadow(&self) -> Option<NoShadowRoot<'a>> {
        None
    }

    /// The language this element itself declares: `xml:lang`, the one in the
    /// XML namespace, before `lang`, which it outranks where both are set (HTML,
    /// "The `lang` and `xml:lang` attributes"). An `xml:lang` written on an HTML
    /// element is in no namespace and means nothing.
    fn lang_attr(&self) -> Option<AttrValue> {
        let element = self.element()?;
        element
            .attr_in(&html5ever::ns!(xml), "lang")
            .or_else(|| element.attr("lang"))
            .map(AttrValue::from)
    }

    /// `:lang()` (Selectors 4 §7.2): whether this element's language falls in
    /// `range`.
    ///
    /// The language is the nearest one declared up the tree, or the document's
    /// default when nothing declares one. An empty declaration says the language
    /// is unknown: it stops the walk, and an unknown language is in no range at
    /// all, `:lang("")` included, as Chrome has it.
    fn match_element_lang(
        &self,
        override_lang: Option<Option<AttrValue>>,
        range: &std::boxed::Box<str>,
    ) -> bool {
        let declared = override_lang.unwrap_or_else(|| {
            std::iter::successors(Some(*self), |node| {
                node.parent_element_id().map(|id| node.at(id))
            })
            .find_map(|node| node.lang_attr())
        });
        let language: Option<&str> = match &declared {
            Some(declared) => Some(declared.as_ref()),
            None => self.tree().default_language(),
        };
        language
            .is_some_and(|language| !language.is_empty() && language_range_matches(range, language))
    }

    fn is_html_document_body_element(&self) -> bool {
        self.element()
            .is_some_and(|element| element.name.local.as_ref() == "body")
            && self
                .parent_element_id()
                .and_then(|id| self.at(id).element())
                .is_some_and(|parent| parent.name.local.as_ref() == "html")
    }

    /// `width`, `bgcolor`, `border` and the rest of the presentational attributes.
    ///
    /// They are style written in the markup, and they belong here rather than in a
    /// stylesheet because of where they cascade: below every author rule, so a page
    /// that says anything at all in CSS beats what its own tags asked for, and above
    /// the user-agent sheet, so `<td bgcolor>` beats the default that would have
    /// been transparent. The engine has an origin for exactly this.
    ///
    /// Worked out when the slot was made, by `crate::hints`, and handed over as
    /// they are. A tree with no slots — one built only to match a selector — has
    /// none to hand over.
    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: selectors::matching::VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: selectors::sink::Push<style::applicable_declarations::ApplicableDeclarationBlock>,
    {
        let Some(block) = self.slot().and_then(|slot| slot.hints.as_ref()) else {
            return;
        };
        hints.push(
            style::applicable_declarations::ApplicableDeclarationBlock::from_declarations(
                block.clone(),
                style::rule_tree::CascadeLevel::new(style::rule_tree::CascadeOrigin::PresHints),
                style::stylesheets::layer_rule::LayerOrder::root(),
            ),
        );
    }

    fn local_name(&self) -> &html5ever::LocalName {
        &self
            .element()
            .expect("local_name on a non-element")
            .name
            .local
    }

    fn namespace(&self) -> &html5ever::Namespace {
        &self.element().expect("namespace on a non-element").name.ns
    }

    fn query_container_size(
        &self,
        _display: &style::values::computed::Display,
    ) -> euclid::Size2D<Option<app_units::Au>, euclid::UnknownUnit> {
        // Container queries need layout to have run, and to be re-run when it
        // changes the answer. Reporting no size means they never match, which is
        // the honest reading of "not implemented".
        euclid::Size2D::new(None, None)
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.slot().is_some_and(|slot| {
            let stored = slot
                .selector_flags
                .load(std::sync::atomic::Ordering::Relaxed);
            ElementSelectorFlags::from_bits_truncate(stored).contains(flags)
        })
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        self.slot().map_or(ElementSelectorFlags::empty(), |slot| {
            let stored = slot
                .selector_flags
                .load(std::sync::atomic::Ordering::Relaxed);
            ElementSelectorFlags::from_bits_truncate(stored)
                & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING
        })
    }
}
