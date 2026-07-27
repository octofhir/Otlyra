//! XPath, asked the way a locator asks it.
//!
//! Out here rather than beside the engine because every case worth writing is
//! written against *markup* — and the parser that turns markup into a document
//! is a development cycle away, which Cargo allows for a test and not for the
//! library. Forty calls to the mutator would be a different document from the
//! one a page produces, and the difference is exactly where the bugs are.

use otlyra_dom::NodeData;
use otlyra_dom::tree::Document;
use otlyra_dom::xpath::{evaluate_to_string, select};

/// The document every test below asks about.
const PAGE: &str = r#"<html><body>
        <h1 id="top" class="title big">Catalogue</h1>
        <div lang="en-GB"><span id="inner">British</span></div>
        <ul class="things">
            <li class="thing">A thing</li>
            <li class="thing chosen">Another thing</li>
            <li class="thing">A third</li>
        </ul>
        <p>Some <a href="/one" class="link">first link</a> and
           <a href="/two">second link</a>.</p>
        <table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>
        </body></html>"#;

fn document() -> Document {
    otlyra_html::parse(PAGE.as_bytes(), Some("utf-8")).document
}

fn find(expression: &str) -> Vec<String> {
    let document = document();
    select(&document, expression)
        .unwrap_or_else(|error| panic!("{expression}: {error}"))
        .into_iter()
        .map(|node| {
            let data = document.node(node);
            match &data.data {
                NodeData::Element(element) => element.name.local.as_ref().to_owned(),
                NodeData::Text(text) => format!("{:?}", text.trim()),
                other => format!("{other:?}"),
            }
        })
        .collect()
}

fn text_of(expression: &str) -> String {
    let document = document();
    let root = document.root();
    evaluate_to_string(&document, root, expression).expect("a value")
}

#[test]
fn a_descendant_search_finds_every_one() {
    assert_eq!(find("//li"), vec!["li", "li", "li"]);
}

#[test]
fn an_absolute_path_walks_the_tree_exactly() {
    assert_eq!(find("/html/body/ul/li"), vec!["li", "li", "li"]);
    // A step that does not match stops the path rather than skipping ahead.
    assert!(find("/html/ul/li").is_empty());
}

#[test]
fn an_attribute_test_is_the_common_locator() {
    assert_eq!(find("//*[@id='top']"), vec!["h1"]);
    assert_eq!(find("//a[@href='/two']"), vec!["a"]);
    // Existence, with no value to compare.
    assert_eq!(find("//a[@class]"), vec!["a"]);
}

#[test]
fn a_predicate_by_number_is_a_position() {
    assert_eq!(find("//li[2]"), vec!["li"]);
    assert_eq!(text_of("//li[2]"), "Another thing");
    assert_eq!(text_of("//li[last()]"), "A third");
    // Two predicates each see the positions the one before left.
    assert!(find("//li[1][2]").is_empty());
}

#[test]
fn text_can_be_matched_which_is_why_drivers_reach_for_xpath() {
    // The expression a person writes when the element has no id and no class
    // worth naming — and the reason a browser without XPath fails them.
    assert_eq!(find("//li[text()='Another thing']"), vec!["li"]);
    assert_eq!(find("//a[contains(text(), 'second')]"), vec!["a"]);
    assert_eq!(find("//*[normalize-space(text())='A third']"), vec!["li"]);
}

#[test]
fn a_class_among_several_is_matched_by_the_padded_trick() {
    // Every driver writes this one, and it only works if a comparison against
    // a node-set is existential and `concat` and `contains` are both right.
    assert_eq!(
        find("//li[contains(concat(' ', normalize-space(@class), ' '), ' chosen ')]"),
        vec!["li"]
    );
}

#[test]
fn the_axes_go_where_they_say() {
    assert_eq!(find("//li[@class='thing chosen']/parent::ul"), vec!["ul"]);
    assert_eq!(
        find("//li[@class='thing chosen']/following-sibling::li"),
        vec!["li"]
    );
    assert_eq!(
        find("//li[@class='thing chosen']/preceding-sibling::li"),
        vec!["li"]
    );
    // Two cells, one table: a node-set has no repeats, so the shared ancestor
    // appears once however many members reached it.
    assert_eq!(find("//td[1]/ancestor::table"), vec!["table"]);
    assert_eq!(find("//h1/self::h1"), vec!["h1"]);
}

#[test]
fn position_on_a_reverse_axis_counts_from_the_context_node() {
    // The rule an implementation gets wrong: this is the sibling *nearest*
    // the third item, which is the second, not the first in the document.
    assert_eq!(text_of("//li[3]/preceding-sibling::li[1]"), "Another thing");
    assert_eq!(text_of("//li[3]/preceding-sibling::li[2]"), "A thing");
}

#[test]
fn a_union_is_both_sides_in_document_order_without_repeats() {
    let both = find("//h1 | //a");
    assert_eq!(both, vec!["h1", "a", "a"]);
    // The same node from both sides appears once.
    assert_eq!(find("//h1 | //h1"), vec!["h1"]);
}

#[test]
fn boolean_and_numeric_operators_work_where_a_predicate_wants_them() {
    assert_eq!(find("//li[position() > 1 and position() < 3]"), vec!["li"]);
    assert_eq!(find("//li[not(@class='thing')]"), vec!["li"]);
    assert_eq!(text_of("count(//li)"), "3");
    assert_eq!(text_of("count(//li) * 2 + 1"), "7");
}

#[test]
fn an_attribute_selected_on_its_own_is_not_handed_back_as_its_element() {
    // `//a/@href` is a set of attributes. A caller holding node handles gets
    // nothing rather than the elements, which would be a different answer to
    // a different question.
    let document = document();
    assert!(select(&document, "//a/@href").expect("valid").is_empty());
    // But its value is there for anything that asks for a string.
    assert_eq!(text_of("//a/@href"), "/one");
    assert_eq!(text_of("string(//a[2]/@href)"), "/two");
}

#[test]
fn a_tag_name_is_matched_however_it_was_typed() {
    // HTML lower-cases its tags and locators are written both ways.
    assert_eq!(find("//LI"), vec!["li", "li", "li"]);
}

#[test]
fn an_expression_that_is_not_one_says_where_it_went_wrong() {
    let document = document();
    for bad in ["//li[", "//", "//li[@id=]", "&&", "//li[1"] {
        let error = select(&document, bad);
        assert!(error.is_err(), "{bad:?} should not have parsed");
    }
}

#[test]
fn an_element_can_be_found_by_id_the_way_xpath_spells_it() {
    assert_eq!(find("id('top')"), vec!["h1"]);
    // Several names in one string, which is what makes this not a selector.
    assert_eq!(find("id('top inner')"), vec!["h1", "span"]);
    assert!(find("id('nowhere')").is_empty());
}

#[test]
fn lang_is_inherited_and_matched_on_the_hyphen() {
    // The rule that makes it worth having: `lang("en")` is true of `en-GB`, and
    // it is asked of the nearest ancestor that says anything.
    assert_eq!(find("//span[lang('en')]"), vec!["span"]);
    assert_eq!(find("//span[lang('en-GB')]"), vec!["span"]);
    assert!(find("//span[lang('fr')]").is_empty());
    // A prefix that is not a whole subtag does not match.
    assert!(find("//span[lang('e')]").is_empty());
    assert!(find("//li[lang('en')]").is_empty());
}

#[test]
fn what_is_not_supported_says_so_rather_than_matching_nothing() {
    let document = document();
    // An empty node-set would read as *nothing on this page matched*, which
    // would send whoever wrote it looking at the wrong thing.
    let error = select(&document, "//li[$wanted]").unwrap_err();
    assert!(error.message.contains("variable"), "{error}");

    let error = select(&document, "//namespace::x").unwrap_err();
    assert!(error.message.contains("namespace"), "{error}");

    let error = select(&document, "//li[foo()]").unwrap_err();
    assert!(error.message.contains("not a function"), "{error}");
}

#[test]
fn a_locator_that_is_not_a_node_set_is_refused_as_a_locator() {
    let document = document();
    // `count(//li)` is a perfectly good expression and a useless locator, and
    // a client that sent one is better told than handed an empty list.
    let error = select(&document, "count(//li)").unwrap_err();
    assert!(error.message.contains("node-set"), "{error}");
}

#[test]
fn the_string_functions_answer_what_the_specification_says_they_do() {
    assert_eq!(text_of("substring('12345', 2, 3)"), "234");
    // The rounding case that catches an implementation out.
    assert_eq!(text_of("substring('12345', 0, 3)"), "12");
    assert_eq!(text_of("substring-before('a/b', '/')"), "a");
    assert_eq!(text_of("substring-after('a/b', '/')"), "b");
    assert_eq!(text_of("translate('bar', 'abc', 'ABC')"), "BAr");
    assert_eq!(text_of("normalize-space('  a   b  ')"), "a b");
    assert_eq!(text_of("string-length('abcd')"), "4");
    assert_eq!(text_of("concat('a', 'b', 'c')"), "abc");
}

#[test]
fn a_number_reads_the_way_xpath_writes_one() {
    // Not "3.0", which is what a naive formatter would print and what would
    // then fail every string comparison against a count.
    assert_eq!(text_of("count(//li)"), "3");
    assert_eq!(text_of("1 div 0"), "Infinity");
    assert_eq!(text_of("string(1.5)"), "1.5");
}
