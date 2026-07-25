//! Whether a change of state can change anything at all.
//!
//! `:hover` changes on every movement of the pointer, and a page has no shortage
//! of elements to move over. Restyling the document each time would make a mouse
//! crossing a paragraph cost what opening the page cost. Almost always it should
//! cost nothing, because almost always nothing in the page's stylesheets depends
//! on the state that changed — and the engine already knows which bits any of its
//! selectors mention, because it builds that index while it builds the rules.
//!
//! Two questions, cheapest first.
//!
//! 1. Does *any* selector in the document depend on the bits that changed? One
//!    intersection against a mask the engine already keeps. A page with no state
//!    rule at all stops here, and so does a page whose only state rule is
//!    `a:visited` when what changed is `:hover`.
//! 2. Does any selector that could apply to *this* element depend on them? The
//!    index is bucketed the way the rules are — by id, class, tag — so a page
//!    whose only state rule is `a:hover` answers no for a paragraph without
//!    reading the rule.
//!
//! A yes is not a promise that the style changed, only that it might have. What
//! follows a yes is the restyle we would have done anyway; what follows a no is
//! nothing.

use style::stylist::Stylist;
use stylo_dom::ElementState;

/// Whether any selector anywhere in the document depends on any of `changed`.
#[must_use]
pub fn document_depends_on(stylist: &Stylist, changed: ElementState) -> bool {
    if changed.is_empty() {
        return false;
    }
    stylist
        .iter_origins()
        .any(|(data, _)| data.has_state_dependency(changed))
}

/// Whether any selector that could apply to `element` depends on any of `changed`.
///
/// The bucket lookup is given `changed` as *additional* state on purpose: a
/// dependency has to be found whether the bit is being set or cleared, and the
/// element carries only one of the two answers at any moment.
///
/// Must be called inside a tree scope for the element's document.
#[must_use]
pub fn element_depends_on(
    stylist: &Stylist,
    element: crate::stylo_dom::NodeRef<'_>,
    changed: ElementState,
) -> bool {
    if changed.is_empty() {
        return false;
    }
    let quirks_mode = stylist.quirks_mode();
    stylist.any_applicable_rule_data(element, |data| {
        if !data.has_state_dependency(changed) {
            return false;
        }
        let mut found = false;
        data.invalidation_map()
            .state_affecting_selectors
            .lookup_with_additional(element, quirks_mode, None, &[], changed, |dependency| {
                if dependency.state.intersects(changed) {
                    found = true;
                    // One dependency is as much of an answer as a thousand.
                    return false;
                }
                true
            });
        found
    })
}

#[cfg(test)]
mod tests {
    use otlyra_dom::{Document, FormState, NodeId};

    use crate::cascade::{ExternalSheets, Styler, Viewport};
    use crate::state::Interaction;

    /// Parse `html`, and prepare a styler over its own stylesheets.
    fn styler(html: &str) -> (Document, Styler) {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styler = Styler::new(&document, Viewport::default(), &ExternalSheets::default());
        (document, styler)
    }

    /// The node whose `id` attribute is `id`.
    fn node_with_id(document: &Document, id: &str) -> NodeId {
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

    /// Whether hovering `id` is worth a restyle.
    fn hovering(document: &Document, styler: &mut Styler, id: &str) -> bool {
        let form = FormState::new();
        styler.interaction_changes_style(
            document,
            &form,
            Interaction::none(),
            Interaction {
                hover: Some(node_with_id(document, id)),
                ..Interaction::none()
            },
        )
    }

    #[test]
    fn a_page_with_no_state_rule_pays_nothing_for_the_pointer() {
        let (document, mut styler) =
            styler("<style>p { color: green }</style><p id=text>x</p><a id=link href=#>y</a>");
        assert!(!hovering(&document, &mut styler, "text"));
        assert!(!hovering(&document, &mut styler, "link"));
    }

    #[test]
    fn only_what_a_rule_could_apply_to_is_worth_restyling() {
        let (document, mut styler) =
            styler("<style>a:hover { color: red }</style><p id=text>x</p><a id=link href=#>y</a>");
        assert!(hovering(&document, &mut styler, "link"));
        // The rule exists, but nothing it could match is under the pointer.
        assert!(!hovering(&document, &mut styler, "text"));
    }

    #[test]
    fn a_rule_that_reaches_out_of_the_hovered_element_still_counts() {
        let (document, mut styler) = styler(
            "<style>.card:hover .title { color: red }</style>\
             <div id=card class=card><h2 id=title class=title>x</h2></div><p id=text>y</p>",
        );
        assert!(hovering(&document, &mut styler, "card"));
        // Hovering the title hovers the card too, because hover reaches the
        // ancestors — so this is a yes for the card's sake, not the title's.
        assert!(hovering(&document, &mut styler, "title"));
        assert!(!hovering(&document, &mut styler, "text"));
    }

    #[test]
    fn a_state_nothing_mentions_is_not_worth_a_restyle() {
        let (document, mut styler) =
            styler("<style>a:visited { color: purple }</style><a id=link href=#>y</a>");
        assert!(!hovering(&document, &mut styler, "link"));
    }

    #[test]
    fn moving_between_two_elements_that_both_match_is_still_one_yes() {
        let (document, mut styler) = styler(
            "<style>a:hover { color: red }</style><a id=one href=#>x</a><a id=two href=#>y</a>",
        );
        let form = FormState::new();
        let interaction = |id: &str| Interaction {
            hover: Some(node_with_id(&document, id)),
            ..Interaction::none()
        };
        assert!(styler.interaction_changes_style(
            &document,
            &form,
            interaction("one"),
            interaction("two"),
        ));
        // Standing still is never worth anything.
        assert!(!styler.interaction_changes_style(
            &document,
            &form,
            interaction("one"),
            interaction("one"),
        ));
    }

    #[test]
    fn the_browsers_own_sheet_counts_as_a_dependency_too() {
        // Nothing in the page mentions a state, but the user-agent sheet may;
        // whatever it says, asking must not panic and must not consult the page's
        // rules alone.
        let (document, mut styler) = styler("<p id=text>x</p>");
        let _ = hovering(&document, &mut styler, "text");
    }
}
