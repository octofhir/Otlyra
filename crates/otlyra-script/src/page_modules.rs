//! Module scripts: the document's list of them, their graphs, and when each
//! one runs.
//!
//! The parser hands every `<script type="module">` to [`PageScripts`] and
//! carries on: a module never stops the parse. Each one is an entry in the
//! page's [`WebModuleMap`], and its graph is fetched by the browser's network
//! — this side only says which addresses it needs, through
//! [`PageScripts::take_module_fetches`], and is told what came back, through
//! [`PageScripts::deliver_module`]. When a graph is whole the module runs:
//!
//! - an ordinary module after the parse, in document order, and the
//!   document's `DOMContentLoaded` waits for all of them;
//! - an `async` one as soon as its graph is in, in whatever order that is.
//!
//! A module's fetch that failed fires `error` at its element. One that does
//! not parse or names a specifier nothing resolves, and one that throws while
//! it runs, are reported to the window as an `ErrorEvent` and to the console,
//! which is what the HTML standard does with an exception in a module. An
//! external module that ran fires `load` at its element. While a module runs,
//! `document.currentScript` is `null`.
//!
//! `import()` takes the same road: the engine parks the call, its fetches are
//! handed out with the rest, and a delivery resumes it.

use otlyra_dom::{Document, NodeId};
use otlyra_html::ModuleResponse;
use otter_runtime::web_modules::{WebModuleError, WebModuleGraph, WebModuleMap, WebModuleSource};

use super::PageScripts;

/// The module scripts of one document that have not run yet.
#[derive(Debug, Default)]
pub(super) struct ModuleScripts {
    /// How many inline modules the document has had: each is its own record.
    inline_count: u64,
    /// Waiting to run, in document order.
    waiting: Vec<ModuleScript>,
    /// Addresses found since the browser last asked.
    fetches: Vec<String>,
    /// Whether the parse is over, which is when ordinary modules may run.
    parsed: bool,
    /// Whether `DOMContentLoaded` is waiting for ordinary modules to run.
    content_loaded_owed: bool,
}

#[derive(Debug)]
struct ModuleScript {
    element: NodeId,
    /// Its key in the module map: the resolved `src`, or the document's URL
    /// with an inline marker.
    key: String,
    /// The text of an inline one; an external one's is in the map.
    inline: Option<String>,
    is_async: bool,
}

/// What became of a module script whose graph is settled.
enum Settled {
    Run,
    FetchFailed(String),
    Invalid(String),
}

impl ModuleScripts {
    /// Whether a module script of this document has still to run.
    pub(super) fn pending(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// Whether ordinary module scripts are what `DOMContentLoaded` waits for.
    pub(super) fn holds_content_loaded(&self) -> bool {
        self.waiting.iter().any(|script| !script.is_async)
    }
}

impl PageScripts {
    fn module_map(&self) -> Option<WebModuleMap> {
        self.host.as_ref().map(|host| host.modules().clone())
    }

    /// The parser met `<script type="module">`.
    pub(super) fn add_module_script(&mut self, element: NodeId, document: &mut Document) {
        let Some(map) = self.module_map() else {
            return;
        };
        self.seen += 1;
        let is_async = document.attr(element, "async").is_some();
        let script = if let Some(src) = document.attr(element, "src") {
            let resolved = url::Url::parse(&self.document_url)
                .and_then(|base| base.join(src.trim()))
                .map(|url| url.to_string());
            let Ok(key) = resolved else {
                // An address that is not one fails the fetch before it starts.
                tracing::warn!(target: "page.script", src, "a module script's src is not a URL");
                self.fire_element_event(document, element, "error");
                return;
            };
            ModuleScript {
                element,
                key,
                inline: None,
                is_async,
            }
        } else {
            let text = element_text(document, element);
            let key = WebModuleMap::inline_url(&self.document_url, self.modules.inline_count);
            self.modules.inline_count += 1;
            map.deliver(
                &key,
                Ok(WebModuleSource {
                    final_url: key.clone(),
                    content_type: Some("text/javascript".to_owned()),
                    text: text.clone(),
                }),
            );
            ModuleScript {
                element,
                key,
                inline: Some(text),
                // `async` on an inline module makes it run as soon as its
                // imports are in, rather than after the parse.
                is_async,
            }
        };
        self.modules.waiting.push(script);
        self.advance_modules(document);
    }

    /// The parser met `<script type="importmap">`.
    pub(super) fn add_import_map(&mut self, element: NodeId, document: &mut Document) {
        let Some(map) = self.module_map() else {
            return;
        };
        if document.attr(element, "src").is_some() {
            // External import maps are not part of the standard.
            tracing::warn!(target: "page.script", "an import map with a src is ignored");
            self.fire_element_event(document, element, "error");
            return;
        }
        let text = element_text(document, element);
        match map.register_import_map(&text, &self.document_url) {
            Ok(warnings) => {
                for warning in warnings {
                    tracing::warn!(target: "page.script", %warning, "import map entry ignored");
                }
            }
            Err(error) => {
                self.report_error(
                    document,
                    &format!("TypeError: {error}"),
                    &self.document_url.clone(),
                );
            }
        }
    }

    /// The browser's network answered for one module.
    pub(super) fn module_arrived(
        &mut self,
        document: &mut Document,
        url: &str,
        response: Result<ModuleResponse, String>,
    ) {
        let Some(map) = self.module_map() else {
            return;
        };
        map.deliver(
            url,
            response.map(|response| WebModuleSource {
                final_url: response.final_url,
                content_type: response.content_type,
                text: response.text,
            }),
        );
        self.advance_modules(document);
    }

    /// Addresses the page's modules need that nobody has asked for yet.
    pub(super) fn module_fetches(&mut self) -> Vec<String> {
        let mut fetches = std::mem::take(&mut self.modules.fetches);
        if let Some(host) = self.host.as_ref() {
            fetches.extend(host.import_fetches());
        }
        fetches
    }

    /// The parse is over: ordinary modules may run now, and say whether
    /// `DOMContentLoaded` must wait for them.
    pub(super) fn modules_after_parse(&mut self, document: &mut Document) -> bool {
        self.modules.parsed = true;
        self.advance_modules(document);
        let holds = self.modules.holds_content_loaded();
        self.modules.content_loaded_owed |= holds;
        holds
    }

    /// Run every module script whose turn it is, resume the `import()` calls
    /// whose modules arrived, and collect what is still to fetch.
    fn advance_modules(&mut self, document: &mut Document) {
        let Some(map) = self.module_map() else {
            return;
        };
        loop {
            let mut next = None;
            let mut blocked_in_order = false;
            for (index, script) in self.modules.waiting.iter().enumerate() {
                let status = map.graph_status(&script.key);
                if let Ok(WebModuleGraph::Pending { fetch }) = &status {
                    self.modules.fetches.extend(fetch.iter().cloned());
                }
                let settled = match status {
                    Ok(WebModuleGraph::Pending { .. }) => None,
                    Ok(WebModuleGraph::Ready) => Some(Settled::Run),
                    Err(error @ WebModuleError::Fetch { .. }) => {
                        Some(Settled::FetchFailed(error.to_string()))
                    }
                    Err(error) => Some(Settled::Invalid(error.to_string())),
                };
                if script.is_async {
                    if next.is_none() && settled.is_some() {
                        next = settled.map(|settled| (index, settled));
                    }
                } else if !blocked_in_order {
                    // Ordinary modules go in document order, after the parse:
                    // the first of them that cannot go holds up the rest.
                    match settled {
                        Some(settled) if self.modules.parsed && next.is_none() => {
                            next = Some((index, settled));
                        }
                        _ => blocked_in_order = true,
                    }
                }
            }
            let Some((index, settled)) = next else {
                break;
            };
            let script = self.modules.waiting.remove(index);
            self.settle_module(document, &map, script, settled);
        }
        if let Some(host) = self.host.as_mut() {
            let resumed = crate::dom::loan(document, &mut self.state, || host.resume_imports());
            self.absorb_turn();
            if let Err(error) = resumed {
                self.failed += 1;
                tracing::error!(target: "page.script", %error, "a dynamic import failed");
            }
        }
        if self.modules.parsed
            && self.modules.content_loaded_owed
            && !self.modules.holds_content_loaded()
        {
            self.modules.content_loaded_owed = false;
            PageScripts::document_finished(self, document, true);
        }
    }

    fn settle_module(
        &mut self,
        document: &mut Document,
        map: &WebModuleMap,
        script: ModuleScript,
        settled: Settled,
    ) {
        match settled {
            Settled::FetchFailed(message) => {
                self.failed += 1;
                tracing::warn!(target: "page.script", %message, "a module script's graph failed to load");
                self.fire_element_event(document, script.element, "error");
            }
            Settled::Invalid(message) => {
                self.failed += 1;
                let url = webby_url(&script.key);
                tracing::error!(target: "page.script", %url, %message, "a module script cannot run");
                self.report_error(document, &message, &url);
            }
            Settled::Run => {
                let external = script.inline.is_none();
                let (source, url) = match script.inline {
                    Some(text) => (text, script.key.clone()),
                    None => match map.source(&script.key) {
                        Some(source) => (source.text, source.final_url),
                        None => return,
                    },
                };
                let Some(host) = self.host.as_mut() else {
                    return;
                };
                // `document.currentScript` is `null` while a module runs.
                let outcome =
                    crate::dom::loan(document, &mut self.state, || host.run_module(&source, &url));
                self.absorb_turn();
                match outcome {
                    Ok(outcome) => {
                        tracing::debug!(
                            target: "page.script",
                            url = webby_url(&url),
                            micros = outcome.duration.as_micros(),
                            "module ran",
                        );
                        if external {
                            self.fire_element_event(document, script.element, "load");
                        }
                    }
                    Err(error) => {
                        self.failed += 1;
                        if error.interrupted {
                            tracing::warn!(target: "page.script", %error, "module stopped by the watchdog");
                        }
                        let message = uncaught_message(&error);
                        tracing::error!(
                            target: "page.script",
                            %error,
                            frames = ?error.frames(),
                            "module script failed",
                        );
                        self.report_error(document, &message, &webby_url(&url));
                    }
                }
            }
        }
    }

    /// Fire a plain event at a script element, as its own turn.
    fn fire_element_event(&mut self, document: &mut Document, element: NodeId, kind: &str) {
        let _ = self.dispatch_input(document, element, kind, "{}");
    }

    /// Report an exception to the window, as an `ErrorEvent`, and to the
    /// console unless a listener cancels it.
    fn report_error(&mut self, document: &mut Document, message: &str, url: &str) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        let source = format!(
            "__otlyraReportError({}, {})",
            super::js_string(message),
            super::js_string(url)
        );
        let outcome = crate::dom::loan(document, &mut self.state, || {
            host.run_classic_script(&source, "<report error>")
        });
        self.absorb_turn();
        if let Err(error) = outcome {
            tracing::error!(target: "page.script", %error, "an error listener failed");
        }
    }
}

/// The text an inline script element carries.
fn element_text(document: &Document, element: NodeId) -> String {
    let mut text = String::new();
    let mut child = document.node(element).first_child();
    while let Some(id) = child {
        if let otlyra_dom::NodeData::Text(chunk) = &document.node(id).data {
            text.push_str(chunk);
        }
        child = document.node(id).next_sibling();
    }
    text
}

/// A module key as the page knows it: an inline module is its document.
fn webby_url(key: &str) -> String {
    otter_runtime::web_modules::strip_inline_marker(key).to_owned()
}

/// What `window.onerror` is told: `Uncaught TypeError: boom`.
fn uncaught_message(error: &crate::host::ScriptError) -> String {
    let text = match &error.source {
        otter_runtime::OtterError::Runtime { diagnostic } => diagnostic.message.clone(),
        other => other.to_string(),
    };
    let first = text.lines().next().unwrap_or_default();
    let thrown = first.strip_prefix("uncaught exception: ").unwrap_or(first);
    format!("Uncaught {thrown}")
}
