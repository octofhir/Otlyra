//! What moving the pointer costs, against what restyling would have cost.
//!
//! The pointer moves tens of times a second and almost never changes anything.
//! This measures the difference between finding that out and assuming otherwise,
//! on a page of four hundred cards — about what a long article or a busy dashboard
//! comes to.
//!
//! ```text
//! cargo run --release -p otlyra-css --example hover-cost
//! ```
//!
//! Three answers are timed. A page with no state rule at all, which should stop at
//! one intersection. A page whose only state rule cannot apply to what is under
//! the pointer, which should stop at the bucket lookup. And a page whose rule does
//! apply, which is the case that has to go on and restyle.

use std::time::Instant;

use otlyra_css::cascade::{ExternalSheets, Styler, Viewport};
use otlyra_css::state::Interaction;
use otlyra_dom::{Document, FormState, NodeId};

/// How many times each answer is asked, so that one answer is measurable.
const ROUNDS: usize = 200;

fn main() {
    let viewport = Viewport {
        width: 1200.0,
        height: 900.0,
        ..Viewport::default()
    };

    for (label, style) in [
        ("no state rule at all", "p { color: #222 }"),
        ("a rule that cannot apply", "table:hover { color: red }"),
        ("a rule that does apply", "p:hover { color: red }"),
    ] {
        let source = page(style);
        let document = otlyra_html::parse(source.as_bytes(), Some("utf-8")).document;
        let mut styler = Styler::new(&document, viewport, &ExternalSheets::default());
        let form = FormState::new();

        // Warm the stylist, and count what a full restyle costs for comparison.
        let started = Instant::now();
        let styled = styler.style(&document);
        let restyle = started.elapsed();
        let elements = styled.len();
        drop(styled);

        let targets = paragraphs(&document);
        let started = Instant::now();
        let mut answers = 0usize;
        for round in 0..ROUNDS {
            let before = Interaction {
                hover: Some(targets[round % targets.len()]),
                ..Interaction::none()
            };
            let after = Interaction {
                hover: Some(targets[(round + 1) % targets.len()]),
                ..Interaction::none()
            };
            if styler.interaction_changes_style(&document, &form, before, after) {
                answers += 1;
            }
        }
        let each = started.elapsed() / ROUNDS as u32;

        println!(
            "{label:26} {each:>10.3?} per move, {} of {ROUNDS} needed a restyle \
             (a restyle of {elements} elements costs {restyle:.3?})",
            answers
        );
    }
}

/// Every `<p>` in the document, in tree order.
fn paragraphs(document: &Document) -> Vec<NodeId> {
    let mut found = Vec::new();
    let mut stack = vec![document.root()];
    while let Some(id) = stack.pop() {
        if document
            .get(id)
            .and_then(|node| node.element())
            .is_some_and(|element| element.name.local.as_ref() == "p")
        {
            found.push(id);
        }
        stack.extend(document.children(id));
    }
    found
}

/// Four hundred cards, with one extra rule.
fn page(extra: &str) -> String {
    let mut out = format!(
        "<!doctype html><meta charset=utf-8><style>\
         body{{margin:0;font:14px/1.4 Times}}\
         .card{{border:1px solid #ccc;padding:8px;margin:6px;background:#f7f7f9}}\
         .row{{display:flex;gap:8px}} .row>div{{flex:1}}\
         .g{{display:grid;grid-template-columns:repeat(4,1fr);gap:6px}}\
         {extra}\
         </style><body>"
    );
    for index in 0..400 {
        out.push_str(&format!(
            "<div class=card><h3>Heading {index}</h3>\
             <p>Some prose with <b>bold</b>, <i>italic</i> and a <a href=#>link</a> in it, \
             long enough to wrap across a couple of lines on an ordinary window width.</p>\
             <div class=row><div>left {index}</div><div>middle</div><div>right</div></div>\
             <div class=g><div>a</div><div>b</div><div>c</div><div>d</div></div></div>"
        ));
    }
    out
}
