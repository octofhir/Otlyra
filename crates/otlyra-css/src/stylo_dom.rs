//! Our DOM, seen through Stylo's eyes.
//!
//! Stylo does not own a DOM; it operates over one through traits, which is what
//! lets Servo, Gecko-derived code and now this browser share a cascade. The price
//! is this file: a handle type that is `Copy`, and the trait implementations that
//! answer every question the style system asks about a node.
//!
//! This is the first half — [`selectors::Element`], which is what matching needs.
//! It is useful on its own: `div.note > p:first-child` can be matched against a
//! parsed document with Servo's own engine before any cascade exists. The rest of
//! `TElement` — element data, restyle damage, the traversal — follows it.

use std::fmt;

use otlyra_dom::{Document, ElementData, NodeId};
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::matching::{ElementSelectorFlags, MatchingContext};
use selectors::{Element as SelectorsElement, OpaqueElement};
use style::selector_parser::{
    AttrValue, NonTSPseudoClass, PseudoElement, SelectorImpl as ServoSelectorImpl,
};
use style::values::AtomIdent;

/// The names Stylo hands the matcher.
///
/// Element names arrive as the plain interned atom html5ever produces; attribute
/// names arrive inside Stylo's `GenericAtomIdent` newtype around the same atom.
/// The two spellings are not interchangeable to the compiler, which is why both
/// are named here rather than guessed at each call site.
type BorrowedLocalName = <ServoSelectorImpl as selectors::SelectorImpl>::BorrowedLocalName;
type BorrowedNamespace = <ServoSelectorImpl as selectors::SelectorImpl>::BorrowedNamespaceUrl;
type LocalNameIdent = style::values::GenericAtomIdent<html5ever::LocalNameStaticSet>;
type NamespaceIdent = style::values::GenericAtomIdent<html5ever::NamespaceStaticSet>;

/// A borrowed handle to one node: the document, and which node in it.
///
/// `Copy`, because Stylo's traits require it — the style system passes these
/// around by value everywhere. That forces the handle to be an index rather than
/// an owned node, which our arena already is.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    /// The document the node lives in.
    pub document: &'a Document,
    /// Which node.
    pub id: NodeId,
}

impl<'a> NodeRef<'a> {
    /// A handle to `id` in `document`.
    pub fn new(document: &'a Document, id: NodeId) -> Self {
        Self { document, id }
    }

    /// The element data, if this node is an element.
    pub fn element(&self) -> Option<&'a ElementData> {
        self.document.get(self.id)?.element()
    }

    /// The same handle, pointed at another node.
    fn at(&self, id: NodeId) -> Self {
        Self {
            document: self.document,
            id,
        }
    }

    /// The nearest ancestor that is an element.
    fn parent_element_id(&self) -> Option<NodeId> {
        let parent = self.document.get(self.id)?.parent?;
        self.document.get(parent)?.element().map(|_| parent)
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
        self.id == other.id && std::ptr::eq(self.document, other.document)
    }
}

impl Eq for NodeRef<'_> {}

impl SelectorsElement for NodeRef<'_> {
    type Impl = ServoSelectorImpl;

    fn opaque(&self) -> OpaqueElement {
        // An identity the matcher can compare and hash without knowing what it is.
        // Derived from the node's own key rather than a pointer: our nodes live in
        // a slotmap and do not have stable addresses.
        OpaqueElement::new(&self.document.node(self.id).data)
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
        self.document
            .prev_element_sibling(self.id)
            .map(|id| self.at(id))
    }

    fn next_sibling_element(&self) -> Option<Self> {
        self.document
            .next_element_sibling(self.id)
            .map(|id| self.at(id))
    }

    fn first_element_child(&self) -> Option<Self> {
        self.document
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
        // What we can answer without a script engine, a session history or a form
        // model. Everything else is honestly false rather than guessed: a `:hover`
        // that is sometimes true by accident is worse than one that is never true.
        match pseudo_class {
            NonTSPseudoClass::Link | NonTSPseudoClass::AnyLink => self.is_link(),
            NonTSPseudoClass::Visited => false,
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
        self.document.is_empty_element(self.id)
    }

    fn is_root(&self) -> bool {
        // The root element, not the document node: `:root` is `<html>`.
        self.parent_element_id().is_none()
            && self
                .document
                .get(self.id)
                .and_then(|node| node.parent)
                .is_some_and(|parent| parent == self.document.root())
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
/// Servo's own matching engine over our tree — the thing the cascade will use for
/// every rule, exercised here on its own so that "does this selector match this
/// element" is answerable and testable before any cascade exists.
pub fn select(document: &Document, selector: &str) -> Result<Vec<NodeId>, String> {
    use selectors::matching::{
        MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags,
        QuirksMode as SelectorsQuirksMode,
    };
    use style::selector_parser::SelectorParser;
    use style::stylesheets::UrlExtraData;

    // Every stylesheet and every selector list is parsed against a base URL, which
    // is what `url()` inside it would resolve against. A selector has none, so this
    // is a stand-in that cannot resolve to anything.
    let url = UrlExtraData(servo_arc::Arc::new(
        url::Url::parse("about:blank").expect("about:blank parses"),
    ));
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

    let mut matched = Vec::new();
    let mut stack = vec![document.root()];
    let mut order = Vec::new();
    while let Some(id) = stack.pop() {
        order.push(id);
        let children: Vec<NodeId> = document.children(id).collect();
        stack.extend(children.into_iter().rev());
    }

    for id in order {
        let node = NodeRef::new(document, id);
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
    /// and one written in 2005 does not. They come free with Servo's parser.
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

    #[test]
    fn a_selector_that_does_not_parse_is_an_error_rather_than_a_panic() {
        let document = otlyra_html::parse(b"<p>x", Some("utf-8")).document;
        assert!(select(&document, "p >>> q").is_err());
        assert!(select(&document, "").is_err());
    }
}
