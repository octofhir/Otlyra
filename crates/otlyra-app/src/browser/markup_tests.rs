//! Dynamic HTML reaches the browser's own style, layout and paint pipeline.

use otlyra_platform::{Painter, Viewport};

use super::Browser;
use crate::fetcher::{Loaded, Loader};

struct Page;

fn by_id(document: &otlyra_dom::Document, id: &str) -> Option<otlyra_dom::NodeId> {
    let mut pending = vec![document.root()];
    while let Some(node) = pending.pop() {
        if document.attr(node, "id") == Some(id) {
            return Some(node);
        }
        pending.extend(document.children(node));
    }
    None
}

impl Loader for Page {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            bytes: include_bytes!("../../../../tests/pages/dynamic-markup.html").to_vec(),
            content_type: Some("text/html".to_owned()),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

#[test]
fn dynamic_markup_is_styled_laid_out_and_exposed_by_the_browser() {
    // Otter's debug bootstrap is deeper than a default test thread's stack.
    super::tests::on_content_stack(|| {
        let mut browser = Browser::new(Page);
        browser.navigate("https://markup.example/");
        browser.wait_for_load(std::time::Duration::from_secs(5));
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(900, 700, 1.0),
        );
        let page = browser.page(0).expect("the parsed page");
        let document = page.document();
        for id in ["heading", "cell", "replacement", "template-copy"] {
            let node = by_id(document, id).unwrap_or_else(|| panic!("{id} was not inserted"));
            let box_id = page
                .boxes()
                .box_for(node)
                .unwrap_or_else(|| panic!("{id} has no layout box"));
            assert!(page.rect_of(box_id).is_some(), "{id} was not laid out");
        }
        assert!(by_id(document, "svg-circle").is_some());
        let root = by_id(document, "root").expect("the mount");
        assert!(by_id(document, "placeholder").is_none());
        let text = crate::digest::text(&crate::a11y::describe_page(page), None);
        assert!(text.contains("Dynamic markup works"), "{text}");
        assert!(text.contains("Parsed in table context"), "{text}");
        assert!(text.contains("Outer replacement"), "{text}");
        assert_eq!(
            document.get(root).expect("root").parent,
            by_id(document, "page")
        );
    });
}
