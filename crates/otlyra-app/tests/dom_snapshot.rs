//! The tree `--dump-dom` prints for the sample page, as a snapshot.
//!
//! The page is deliberately malformed — an unclosed paragraph, misnested
//! formatting, a stray end tag, text loose inside a table — so the snapshot
//! records what we do with input nobody wrote on purpose, which is most input.

use std::path::Path;

#[test]
fn the_sample_page_parses_to_a_stable_tree() {
    let path = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/pages/basic.html"
    ));
    let bytes = std::fs::read(path).expect("the sample page");
    let parsed = otlyra_html::parse(&bytes, None);

    assert_eq!(
        parsed.encoding.source,
        otlyra_html::EncodingSource::MetaPrescan,
        "the page declares utf-8 in a meta and nothing else says otherwise"
    );
    insta::assert_snapshot!(otlyra_dom::dump::serialize(&parsed.document));
}
