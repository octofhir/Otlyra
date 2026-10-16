//! `localStorage` and `sessionStorage`: what a document keeps, who else sees
//! it, and what a document with no origin to share keeps.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use otlyra_html::HtmlParser;
use otlyra_script::PageScripts;
use otlyra_script::storage::StorageArea;
use otter_runtime::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle};

#[derive(Debug, Default)]
struct Captured {
    lines: Mutex<Vec<String>>,
}

impl ConsoleSink for Captured {
    fn write(&self, _level: ConsoleLevel, fields: &[String]) {
        self.lines
            .lock()
            .expect("no test panics while holding this")
            .push(fields.join(" "));
    }
}

/// Run `html` as a document at `url`, with `areas` as the browser's storage
/// when given, and answer what it logged.
fn run(url: &str, areas: Option<&[Arc<StorageArea>; 2]>, html: &str) -> Vec<String> {
    let sink = Arc::new(Captured::default());
    let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
    let mut scripts = Box::new(PageScripts::with_console(url, handle));
    if let Some([local, session]) = areas {
        scripts.set_storage(Arc::clone(local), Arc::clone(session));
    }
    let mut parser = HtmlParser::new().with_script_runner(scripts);
    let mut document = otlyra_dom::Document::new();
    parser.feed(&mut document, html.into());
    if let Some(mut runner) = parser.finish(&mut document) {
        runner.settle(&mut document, Duration::from_millis(50));
    }
    let lines = sink
        .lines
        .lock()
        .expect("no test panics while holding this");
    lines.clone()
}

fn areas() -> [Arc<StorageArea>; 2] {
    [Arc::new(StorageArea::new()), Arc::new(StorageArea::new())]
}

#[test]
fn local_storage_is_shared_by_the_origins_documents() {
    let shared = areas();
    let first = run(
        "https://example.com/one",
        Some(&shared),
        "<script>\
           localStorage.setItem('token', 'abc');\
           localStorage.theme = 'dark';\
           sessionStorage.setItem('tab', '1');\
           console.log(localStorage.length, localStorage.token, localStorage.getItem('theme'));\
         </script>",
    );
    assert_eq!(first, ["2 abc dark"]);
    let second = run(
        "https://example.com/two",
        Some(&shared),
        "<script>\
           console.log(localStorage.getItem('token'), 'theme' in localStorage,\
             Object.keys(localStorage).join(','), sessionStorage.getItem('tab'));\
           delete localStorage.theme;\
           console.log(localStorage.length, localStorage.key(0), localStorage.key(5));\
         </script>",
    );
    assert_eq!(second, ["abc true token,theme 1", "1 token null"]);
    let other = run(
        "https://other.test/",
        Some(&shared),
        "<script>console.log(localStorage.length, localStorage.getItem('token'))</script>",
    );
    assert_eq!(other, ["0 null"]);
}

#[test]
fn storage_answers_like_the_platform() {
    let log = run(
        "https://example.com/",
        Some(&areas()),
        "<script>\
           console.log(String(localStorage), localStorage instanceof Storage, typeof localStorage.getItem);\
           localStorage.setItem('getItem', 'shadowed');\
           console.log(typeof localStorage.getItem, localStorage.getItem('getItem'));\
           try { localStorage.setItem('big', 'x'.repeat(6 * 1024 * 1024)); }\
           catch (error) { console.log(error.name, error instanceof DOMException); }\
           console.log(localStorage.getItem('big'));\
           try { new Storage(); } catch (error) { console.log(error.constructor.name); }\
         </script>",
    );
    assert_eq!(
        log,
        [
            "[object Storage] true function",
            "function shadowed",
            "QuotaExceededError true",
            "null",
            "TypeError",
        ]
    );
}

#[test]
fn a_document_with_no_origin_keeps_its_own() {
    let shared = areas();
    let script =
        "<script>console.log(localStorage.getItem('k')); localStorage.setItem('k', 'v');</script>";
    assert_eq!(run("about:blank", Some(&shared), script), ["null"]);
    assert_eq!(run("about:blank", Some(&shared), script), ["null"]);
    assert!(shared[0].origins().is_empty());
}
