//! Module scripts in a parsed page: when they run, what they see, and what
//! happens when one cannot.
//!
//! The network is a table here: whatever module address the page asks for is
//! answered from it, and anything not in it fails the way a 404 does.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use otlyra_html::{HtmlParser, ModuleResponse, ScriptRunner};
use otlyra_script::PageScripts;
use otter_runtime::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle};

const PAGE: &str = "https://example.com/app/";

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

/// One parsed page and the table its modules are fetched from.
struct Page {
    document: otlyra_dom::Document,
    parser: Option<HtmlParser>,
    runner: Option<Box<dyn ScriptRunner>>,
    sink: Arc<Captured>,
    files: Vec<(String, Result<String, String>)>,
    /// Every address the page asked for, in order.
    asked: Vec<String>,
    /// Addresses asked for and not answered yet.
    held: Vec<String>,
}

impl Page {
    fn parse(html: &str, files: &[(&str, &str)]) -> Self {
        let sink = Arc::new(Captured::default());
        let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
        let mut parser =
            HtmlParser::new().with_script_runner(Box::new(PageScripts::with_console(PAGE, handle)));
        let mut document = otlyra_dom::Document::new();
        parser.feed(&mut document, html.into());
        while parser.blocked_on().is_some() {
            parser.skip_script(&mut document);
        }
        Self {
            document,
            parser: Some(parser),
            runner: None,
            sink,
            files: files
                .iter()
                .map(|(url, text)| ((*url).to_owned(), Ok((*text).to_owned())))
                .collect(),
            asked: Vec::new(),
            held: Vec::new(),
        }
    }

    fn runner(&mut self) -> &mut dyn ScriptRunner {
        match self.parser.as_mut() {
            Some(parser) => parser.runner_mut().expect("a runner"),
            None => self.runner.as_deref_mut().expect("a runner"),
        }
    }

    /// Collect what the page asks for, without answering.
    fn collect(&mut self) {
        let fetches = self.runner().take_module_fetches();
        self.asked.extend(fetches.iter().cloned());
        self.held.extend(fetches);
    }

    /// Answer one held address from the table.
    fn answer(&mut self, url: &str) {
        self.collect();
        let at = self
            .held
            .iter()
            .position(|held| held == url)
            .unwrap_or_else(|| panic!("{url} was not asked for: {:?}", self.held));
        self.held.remove(at);
        let response = match self.files.iter().find(|(name, _)| name == url) {
            Some((_, Ok(text))) => Ok(ModuleResponse {
                final_url: url.to_owned(),
                content_type: Some("text/javascript".to_owned()),
                text: text.clone(),
            }),
            Some((_, Err(error))) => Err(error.clone()),
            None => Err("HTTP 404".to_owned()),
        };
        let mut document = std::mem::take(&mut self.document);
        self.runner().deliver_module(&mut document, url, response);
        self.document = document;
    }

    /// Answer everything until the page asks for nothing more.
    fn answer_all(&mut self) {
        loop {
            self.collect();
            let Some(url) = self.held.first().cloned() else {
                return;
            };
            self.answer(&url);
        }
    }

    fn finish(&mut self) {
        let parser = self.parser.take().expect("still parsing");
        self.runner = parser.finish(&mut self.document);
    }

    fn settle(&mut self) {
        let mut document = std::mem::take(&mut self.document);
        self.runner()
            .settle(&mut document, Duration::from_millis(100));
        self.document = document;
    }

    fn log(&self) -> Vec<String> {
        self.sink
            .lines
            .lock()
            .expect("no test panics while holding this")
            .clone()
    }
}

/// Parse, deliver everything, finish, settle.
fn run(html: &str, files: &[(&str, &str)]) -> Vec<String> {
    let mut page = Page::parse(html, files);
    page.answer_all();
    page.finish();
    page.answer_all();
    page.settle();
    page.log()
}

#[test]
fn modules_run_after_the_parse_in_document_order_and_before_content_loaded() {
    let log = run(
        "<script>document.addEventListener('DOMContentLoaded', () => console.log('dcl'))</script>\
         <script type=module>console.log('inline module')</script>\
         <script>console.log('classic')</script>\
         <script type=module src=external.js></script>\
         <p>after</p>",
        &[(
            "https://example.com/app/external.js",
            "console.log('external module', document.querySelector('p').textContent)",
        )],
    );
    assert_eq!(
        log,
        ["classic", "inline module", "external module after", "dcl"]
    );
}

#[test]
fn content_loaded_waits_for_a_module_still_being_fetched() {
    let mut page = Page::parse(
        "<script>document.addEventListener('DOMContentLoaded', () => console.log('dcl'))</script>\
         <script type=module src=slow.js></script>",
        &[("https://example.com/app/slow.js", "console.log('slow')")],
    );
    page.collect();
    page.finish();
    assert!(
        page.log().is_empty(),
        "nothing before the module: {:?}",
        page.log()
    );
    page.answer("https://example.com/app/slow.js");
    assert_eq!(page.log(), ["slow", "dcl"]);
}

#[test]
fn an_async_module_runs_as_soon_as_its_graph_is_in() {
    let mut page = Page::parse(
        "<script type=module src=ordinary.js></script>\
         <script type=module async src=quick.js></script>",
        &[
            (
                "https://example.com/app/ordinary.js",
                "console.log('ordinary')",
            ),
            ("https://example.com/app/quick.js", "console.log('async')"),
        ],
    );
    page.collect();
    page.answer("https://example.com/app/quick.js");
    assert_eq!(page.log(), ["async"], "async ran during the parse");
    page.answer("https://example.com/app/ordinary.js");
    assert_eq!(page.log(), ["async"], "an ordinary one waits for the parse");
    page.finish();
    assert_eq!(page.log(), ["async", "ordinary"]);
}

#[test]
fn a_module_graph_with_static_imports_import_maps_and_a_cycle_runs_once_each() {
    let log = run(
        r#"<script type=importmap>{"imports": {"lib": "/lib/index.js"}}</script>
           <script type=module>import { name } from 'lib'; import './a.js'; console.log('entry', name)</script>
           <script type=module>import { name } from 'lib'; console.log('second', name)</script>"#,
        &[
            (
                "https://example.com/lib/index.js",
                "console.log('lib evaluated'); export const name = 'lib';",
            ),
            (
                "https://example.com/app/a.js",
                "import { b } from 'https://example.com/app/b.js'; export const a = 1; console.log('a', b());",
            ),
            (
                "https://example.com/app/b.js",
                "import { a } from './a.js'; export function b() { return a + 1; } console.log('b');",
            ),
        ],
    );
    assert_eq!(
        log,
        ["lib evaluated", "b", "a 2", "entry lib", "second lib"]
    );
}

#[test]
fn a_nomodule_fallback_does_not_run() {
    let log = run(
        "<script nomodule>console.log('fallback')</script>\
         <script type=module>console.log('module')</script>",
        &[],
    );
    assert_eq!(log, ["module"]);
}

#[test]
fn current_script_is_null_in_a_module_and_import_meta_is_the_document() {
    let log = run(
        "<script id=classic>console.log('classic', document.currentScript.id)</script>\
         <script type=module>console.log('module', document.currentScript, import.meta.url)</script>\
         <script type=module src=dir/m.js></script>",
        &[(
            "https://example.com/app/dir/m.js",
            "console.log('external', import.meta.url)",
        )],
    );
    assert_eq!(
        log,
        [
            "classic classic",
            "module null https://example.com/app/",
            "external https://example.com/app/dir/m.js"
        ]
    );
}

#[test]
fn a_module_that_fails_to_load_fires_error_at_its_element() {
    let log = run(
        "<script type=module src=missing.js onerror=\"console.log('error event')\"></script>\
         <script type=module src=ok.js onload=\"console.log('load event')\"></script>\
         <script type=module>import './gone.js'; console.log('never')</script>",
        &[("https://example.com/app/ok.js", "console.log('ok')")],
    );
    assert!(log.contains(&"error event".to_owned()), "{log:?}");
    assert!(log.contains(&"ok".to_owned()) && log.contains(&"load event".to_owned()));
    assert!(!log.contains(&"never".to_owned()));
}

#[test]
fn an_exception_in_a_module_reaches_window_onerror() {
    let log = run(
        "<script>window.onerror = (message) => { console.log('onerror:', message); return true; }</script>\
         <script type=module>throw new TypeError('boom')</script>\
         <script type=module>import 'bare-name'</script>\
         <script type=module>console.log('still running')</script>",
        &[],
    );
    assert_eq!(log.len(), 3, "{log:?}");
    assert_eq!(log[0], "onerror: Uncaught TypeError: boom");
    assert!(
        log[1].starts_with("onerror: ") && log[1].contains("bare-name"),
        "{log:?}"
    );
    assert_eq!(log[2], "still running");
}

#[test]
fn a_dynamic_import_is_fetched_and_settles_after_delivery() {
    let mut page = Page::parse(
        "<script type=module>\
           import('./lazy.js').then((m) => console.log('lazy', m.value));\
           console.log('requested');\
         </script>",
        &[
            (
                "https://example.com/app/lazy.js",
                "export { value } from './dep.js';",
            ),
            ("https://example.com/app/dep.js", "export const value = 7;"),
        ],
    );
    page.finish();
    assert_eq!(page.log(), ["requested"]);
    page.answer_all();
    assert_eq!(page.log(), ["requested", "lazy 7"]);
    assert_eq!(
        page.asked,
        [
            "https://example.com/app/lazy.js",
            "https://example.com/app/dep.js"
        ]
    );
}

#[test]
fn top_level_await_does_not_hold_content_loaded() {
    let log = run(
        "<script>document.addEventListener('DOMContentLoaded', () => console.log('dcl'))</script>\
         <script type=module>\
           console.log('before await');\
           await new Promise((resolve) => setTimeout(resolve, 10));\
           console.log('after await');\
         </script>",
        &[],
    );
    assert_eq!(log, ["before await", "dcl", "after await"]);
}
