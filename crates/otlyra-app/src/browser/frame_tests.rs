//! Frames: an `<iframe>` loads a document of its own and is drawn into its
//! element, at the element's size, and goes with its page.

use otlyra_dom::{Document, NodeId};
use otlyra_platform::Viewport;

use super::*;
use crate::fetcher::{Loaded, Loader};

/// A parent page with one frame, and the frame's own page.
struct Framed;

impl Loader for Framed {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        let html = match url {
            "https://parent.example/" => {
                "<!doctype html><title>Parent</title><body style='margin:0'>\
                 <iframe src='https://child.example/' width=120 height=60></iframe>"
            }
            "https://child.example/" => {
                "<!doctype html><title>Child</title>\
                 <body style='margin:0;background:rgb(255,0,0)'>"
            }
            "https://framing.example/" => {
                "<!doctype html><title>Framing</title>\
                 <iframe src='https://guarded.example/'></iframe>"
            }
            "https://guarded.example/" => "<!doctype html><title>Guarded</title><p>sign in here",
            _ => "<!doctype html><title>Elsewhere</title><p>no frames",
        };
        let response_headers = if url == "https://guarded.example/" {
            vec![("x-frame-options".to_owned(), "DENY".to_owned())]
        } else {
            Vec::new()
        };
        Ok(Loaded {
            response_headers,
            bytes: html.as_bytes().to_vec(),
            content_type: Some("text/html".to_owned()),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

fn viewport() -> Viewport {
    Viewport::new(800, 600, 1.0)
}

/// Load `url` and draw until the frames it starts have arrived and been
/// drawn, the way a screenshot does.
fn load(browser: &mut Browser, url: &str) {
    browser.navigate(url);
    browser.wait_for_load(std::time::Duration::from_secs(5));
    for _ in 0..4 {
        browser.prepare_frame(viewport(), std::time::Duration::from_secs(5));
        browser.wait_for_load(std::time::Duration::from_secs(5));
    }
    browser.prepare_frame(viewport(), std::time::Duration::from_secs(5));
}

fn iframe(document: &Document) -> NodeId {
    let mut stack = vec![document.root()];
    while let Some(node) = stack.pop() {
        if document
            .get(node)
            .and_then(|dom| dom.element())
            .is_some_and(|element| &*element.name.local == "iframe")
        {
            return node;
        }
        stack.extend(document.children(node));
    }
    panic!("no iframe in the page");
}

#[test]
fn a_frame_loads_its_own_document_into_its_element() {
    let mut browser = Browser::new(Framed);
    load(&mut browser, "https://parent.example/");

    let frames = browser.world.frame_sizes();
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].1, (120.0, 60.0), "the element's content box");

    let tab = browser.tabs[browser.active].id;
    assert_eq!(
        browser.tabs[browser.active].title, "Parent",
        "the frame's title is not the tab's"
    );
    let page = browser.world.page(tab).expect("the parent page");
    let element = iframe(page.document());
    assert_eq!(
        page.picture_source(element).map(|(source, _)| source),
        Some("https://child.example/"),
        "the frame's drawing is its element's picture"
    );
}

#[test]
fn a_frame_goes_with_its_page() {
    let mut browser = Browser::new(Framed);
    load(&mut browser, "https://parent.example/");
    assert_eq!(browser.world.frame_sizes().len(), 1);

    load(&mut browser, "https://elsewhere.example/");
    assert!(browser.world.frame_sizes().is_empty());
}

#[test]
fn a_document_that_refuses_to_be_framed_is_not_shown() {
    let mut browser = Browser::new(Framed);
    load(&mut browser, "https://framing.example/");
    let frames = browser.world.frame_sizes();
    assert_eq!(frames.len(), 1);
    assert!(
        browser.world.page(frames[0].0).is_none(),
        "the refused document was never parsed"
    );
    let tab = browser.tabs[browser.active].id;
    let page = browser.world.page(tab).expect("the framing page");
    assert_eq!(page.picture_source(iframe(page.document())), None);
}
