//! A load, from the request to the last thing the page asked for.
//!
//! The document is asked for, sniffed, parsed — stopping at any script it has to
//! wait for — and shown, and then its stylesheets, pictures and scripts arrive and
//! the page is built for real. It is the longest path in the browser and the only
//! one that holds a `PendingLoad`, so it is kept whole, from the first byte to the
//! last.

use std::collections::HashMap;

use otlyra_css::cascade::ExternalSheets;
use otlyra_dom::NodeId;
use otlyra_layout::Images;

use crate::downloads;
use crate::fetcher::{Body, Fetched, ResourceKind};
use crate::page::{PageScene, title_of};
use crate::ui::SystemPage;

use super::tabs::next_navigation_id;
use super::{Browser, SURFACE_PAGE, SURFACE_SYSTEM};

/// A load in flight, and everything it is still waiting for.
pub(super) struct PendingLoad {
    /// The request the document itself was asked for under.
    pub(super) document: u64,
    /// Whether that request has come back.
    ///
    /// The line between *this tab has a document to look at* and *this tab has
    /// everything it asked for*, which is the line WebDriver draws between
    /// `interactive` and `complete` — and the only reason a driver can be
    /// answered before the last picture arrives.
    pub(super) document_arrived: bool,
    /// Where the tab was before, which decides whether this is a new place.
    previous_url: String,
    /// Whether arriving should add a history entry. A reload and a step through
    /// the history are the same place again, so they do not.
    record: bool,
    /// Where to put the reader once the page is built.
    restore_scroll: f32,
    sheets: ExternalSheets,
    images: Images,
    /// Which file each of those pictures came from, and at what density.
    picture_sources: HashMap<NodeId, (String, f32)>,
    /// What each outstanding request will feed once it arrives.
    outstanding: HashMap<u64, Vec<PendingResource>>,
    /// The parse, when it is stopped part way at a script it is waiting for.
    ///
    /// A page whose parser is stopped is already on screen: everything before
    /// the script is in the tree the tab is showing. What is here is the state
    /// that carries on tokenizing once the script's bytes arrive.
    parse: Option<otlyra_html::HtmlParser>,
    /// The `<script src>` elements, in the order the document names them.
    script_order: Vec<NodeId>,
    /// The source of each of those that has arrived.
    scripts: HashMap<NodeId, String>,
    /// The page's script world, kept alive from the parse until the external
    /// scripts have run in it.
    runner: Option<Box<dyn otlyra_html::ScriptRunner>>,
}

/// A tree and what the parse still owes it, whether or not the parse has ended.
///
/// The tail of a load reads the same three things whether the document is
/// finished or stopped at a script, so both paths hand it this.
struct ParsedSoFar {
    document: otlyra_dom::Document,
    /// The `<script src>` the parse went past without running.
    deferred_scripts: Vec<NodeId>,
}

/// What a subresource is for once it lands.
enum PendingResource {
    /// The `<link>` whose stylesheet this is.
    Stylesheet(NodeId),
    /// The `<img>` whose picture this is, the address it settled on as the
    /// markup spells it, and that candidate's density — which is what the file's
    /// own size is divided by.
    Image(NodeId, String, f32),
    /// The `<script src>` whose source this is.
    Script(NodeId),
    /// A script asked for before the parse, named by the `src` that asked for it.
    ///
    /// There is no node to hang it on yet — the document it is in has not been
    /// parsed, which is the point.
    ScriptSource(String),
}

/// How long a page's own deferred work may run before the first frame.
///
/// Not a policy about timers — the loop runs those — but about *this* moment:
/// the frame the reader is about to see. A page that hydrates and then paints
/// from a zero-delay timer should paint into that frame; one that has scheduled
/// a second of animation should not hold it.
const SETTLE_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

/// Note in the log when a document asked for more than the limit allows.
fn report_limit(asked: usize, limit: usize, what: &str) {
    if asked > limit {
        tracing::warn!(
            asked,
            fetched = limit,
            "the document asks for more {what} than the limit"
        );
    }
}

/// How many stylesheets one document may pull in.
///
/// A limit rather than none: every one of these is a synchronous fetch on the way
/// to the first frame, and a document that asks for hundreds is either generated
/// or hostile.
const STYLESHEET_LIMIT: usize = 32;

/// How many pictures one document may pull in, for the same reason.
pub(super) const IMAGE_LIMIT: usize = 64;

/// How many scripts one page may link to.
///
/// A page with more than this many is not a page we refuse; it is a page whose
/// hundredth script we decline to fetch, the way the stylesheet and picture
/// limits work.
const SCRIPT_LIMIT: usize = 32;

/// How many navigations in a row page script may ask for.
///
/// Long enough for a real bounce chain — a shortener into a sign-in into the
/// page — and short enough that a loop stops.
const SCRIPT_HOP_LIMIT: u8 = 8;

/// The document a picture is shown in.
///
/// A browser given a picture and nothing else wraps it in a document of its own —
/// there is no markup to render, and an `<img>` is what the rest of the engine
/// already knows how to place.
fn image_document(url: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title>\
         <style>html {{ background: #1c1c1e }} \
         body {{ margin: 0; height: 100vh; display: flex; \
         justify-content: center; align-items: center }} \
         img {{ max-width: 100%; max-height: 100% }}</style>\
         <img src=\"{url}\" alt=\"\">",
        name = escape(url.rsplit('/').next().unwrap_or(url)),
        url = escape(url),
    )
}

/// The document text is shown in.
///
/// Text is text: it is wrapped in a `<pre>` so that its own line breaks and spacing
/// survive, and escaped so that a file full of markup is *shown* rather than
/// rendered — which is the whole point of having decided it was not a document.
fn text_document(text: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8>\
         <style>pre {{ font-family: monospace; white-space: pre; margin: 8px }}</style>\
         <pre>{}</pre>",
        escape(text)
    )
}

/// The four characters that would otherwise be markup.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Decode bytes that are not a document: a stylesheet, or a text file being shown
/// as itself.
///
/// A BOM or a charset from the transport decides; anything else is UTF-8, which is
/// CSS's own default and not HTML's — an unlabelled *document* is assumed to be
/// windows-1252, an unlabelled *stylesheet* is not.
fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    let decision = otlyra_html::determine(bytes, charset);
    match decision.source {
        otlyra_html::EncodingSource::Bom | otlyra_html::EncodingSource::TransportCharset => {
            decision.encoding.decode(bytes).0.into_owned()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

impl Browser {
    /// Ask for `url` and leave the tab waiting for it.
    ///
    /// Nothing here waits: the request goes to the fetch thread and the answer
    /// arrives as an event like any other. `record` says whether reaching it should
    /// become a history entry, and `restore_scroll` where the reader should be put
    /// once it has.
    pub(super) fn start_load(
        &mut self,
        url: &str,
        user_initiated: bool,
        record: bool,
        restore_scroll: f32,
    ) {
        self.start_send(url, user_initiated, record, restore_scroll, None);
    }

    /// What the cache is allowed to do about the navigation now being started.
    ///
    /// Set for the one navigation and cleared by it, because a reload is an
    /// instruction about *this* fetch: leaving it on would make every later
    /// click behave like a reload, and a browser that never answers from its
    /// cache is a browser with no cache.
    fn take_cache_mode(&mut self) -> otlyra_net::CacheMode {
        std::mem::take(&mut self.next_cache_mode)
    }

    /// Ask for `url` with a body, and leave the tab waiting for it.
    ///
    /// The same navigation as any other in every respect but the method: the same
    /// scheme policy, the same history entry, the same pending state. What a form
    /// sends is bytes on the request rather than a different way of getting there.
    pub(super) fn start_send(
        &mut self,
        url: &str,
        user_initiated: bool,
        record: bool,
        restore_scroll: f32,
        body: Option<Body>,
    ) {
        let _span = tracing::info_span!("navigation", url).entered();
        // The document a context menu was asked about is being replaced, and
        // its rows name things in it.
        self.ui.dismiss_context_menu();
        self.context_target = None;

        // A browser's own page is fetched from nothing and parsed from nothing:
        // it is a surface this program draws. Catching it in the one place every
        // navigation passes through — the menu, the address bar, the command
        // line, and a step through the history — is what makes it a place a tab
        // can be rather than a mode the window is in.
        if let Some(page) = SystemPage::from_url(url) {
            self.show_system(page, record, restore_scroll);
            return;
        }
        self.activate_surface(SURFACE_PAGE);
        self.tabs[self.active].system = None;

        if !user_initiated && let Ok(target) = otlyra_net::normalize(url) {
            let from = self.tabs[self.active].url.clone();
            if !otlyra_net::may_navigate(Some(&from), &target) {
                tracing::warn!(%url, %from, "navigation refused by scheme policy");
                let tab = &mut self.tabs[self.active];
                tab.error = Some(format!("Refused to open {url} from {from}"));
                tab.page = None;
                tab.pending = None;
                return;
            }
        }

        let previous_url = self.tabs[self.active].url.clone();
        let cache = self.take_cache_mode();
        let id = self.fetcher.fetch(url, ResourceKind::Document, body, cache);
        self.load_started = std::time::Instant::now();

        let tab = &mut self.tabs[self.active];
        tab.url = url.to_owned();
        tab.error = None;
        tab.title = url.to_owned();
        tab.navigation = Some(next_navigation_id());
        tab.pending = Some(PendingLoad {
            document: id,
            document_arrived: false,
            previous_url,
            record,
            restore_scroll,
            sheets: ExternalSheets::default(),
            images: Images::default(),
            picture_sources: HashMap::new(),
            outstanding: HashMap::new(),
            parse: None,
            script_order: Vec::new(),
            scripts: HashMap::new(),
            runner: None,
        });
        // Through `sync_address` rather than straight into the field: the address
        // and whether this page is one the reader kept are two things the toolbar
        // draws from the same fact, and setting one without the other is how the
        // star ends up describing the page before this one.
        self.sync_address();
    }

    /// One finished fetch. Returns whether it changed anything on screen.
    pub(super) fn receive(&mut self, fetched: Fetched) -> bool {
        // A font belongs to the shaper rather than to a page: once it is in, every
        // page that names the family is set in it.
        if let Some(family) = self.font_fetches.remove(&fetched.id) {
            let Ok(loaded) = fetched.result else {
                tracing::warn!(%family, url = %fetched.url, "font failed to load");
                return false;
            };
            if !self.text.add_font(&family, loaded.bytes) {
                tracing::warn!(%family, url = %fetched.url, "font failed to register");
                return false;
            }
            tracing::debug!(%family, url = %fetched.url, "font registered");
            for tab in &mut self.tabs {
                if let Some(page) = tab.page.as_mut() {
                    page.font_arrived();
                }
            }
            return true;
        }

        // A background picture belongs to a page rather than to a load, and may
        // arrive long after the page it is for.
        if let Some((index, url)) = self.background_fetches.remove(&fetched.id) {
            let Ok(loaded) = fetched.result else {
                tracing::warn!(%url, "background picture failed to load");
                return false;
            };
            match otlyra_gfx::decode_image(&loaded.bytes) {
                Ok(picture) => {
                    self.images.insert(fetched.url.clone(), picture.clone());
                    match self.tabs.get_mut(index).and_then(|tab| tab.page.as_mut()) {
                        Some(page) => page.set_picture(url, picture),
                        None => tracing::warn!(%url, "no page to give the picture to"),
                    }
                    return true;
                }
                Err(error) => {
                    tracing::warn!(%url, %error, "background picture failed to decode");
                    return false;
                }
            }
        }

        // A picture an element chose again after the page was built: the same
        // element, a different file.
        if let Some((index, node, src, density)) = self.picture_fetches.remove(&fetched.id) {
            let Ok(loaded) = fetched.result else {
                tracing::warn!(%src, "re-chosen picture failed to load");
                return false;
            };
            match otlyra_gfx::decode_image(&loaded.bytes) {
                Ok(data) => {
                    self.images.insert(fetched.url.clone(), data.clone());
                    match self.tabs.get_mut(index).and_then(|tab| tab.page.as_mut()) {
                        Some(page) => {
                            page.set_image(node, src, otlyra_layout::Picture { data, density })
                        }
                        None => tracing::warn!(%src, "no page to give the picture to"),
                    }
                    return true;
                }
                Err(error) => {
                    tracing::warn!(%src, %error, "re-chosen picture failed to decode");
                    return false;
                }
            }
        }

        let Some(index) = self.tabs.iter().position(|tab| {
            tab.pending.as_ref().is_some_and(|pending| {
                pending.document == fetched.id || pending.outstanding.contains_key(&fetched.id)
            })
        }) else {
            // A load nobody is waiting for any more: the tab moved on, or closed.
            return false;
        };

        match fetched.kind {
            ResourceKind::Document => self.receive_document(index, fetched),
            ResourceKind::Stylesheet | ResourceKind::Image | ResourceKind::Script => {
                self.receive_subresource(index, fetched);
                true
            }
        }
    }

    /// The page itself arrived.
    ///
    /// The document is shown straight away, before its stylesheets and pictures
    /// have been asked for: a page that is readable now and styled a moment later
    /// beats a blank window for the length of the slowest thing it links to.
    fn receive_document(&mut self, index: usize, fetched: Fetched) -> bool {
        let loaded = match fetched.result {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(%error, "navigation failed");
                let tab = &mut self.tabs[index];
                tab.title = "Failed".to_owned();
                tab.page = None;
                tab.error = Some(error);
                tab.pending = None;
                return true;
            }
        };

        // An attachment is a completed download, not a document with unusual
        // bytes. Keep the payload in the browser-owned store and show the page
        // that names the result instead of sending it through MIME sniffing and
        // the HTML parser.
        if let Some(filename) =
            downloads::attachment_filename(&loaded.response_headers, &loaded.final_url)
        {
            let (record, previous_url) = self.tabs[index]
                .pending
                .as_ref()
                .map_or((false, String::new()), |pending| {
                    (pending.record, pending.previous_url.clone())
                });
            let final_url = loaded.final_url;
            let size = loaded.bytes.len();
            let recorded = self.downloads.record(
                filename.clone(),
                final_url.clone(),
                loaded.content_type,
                loaded.bytes,
            );

            // The preference decides here rather than on the page: an automatic
            // download is one nobody pressed anything for, so the write has to
            // start where the bytes arrive.
            if let Some(id) = recorded
                && !self.settings.settings.asks_where_to_save()
                && let Some(directory) = self.settings.settings.download_directory()
                && let Some(bytes) = self.downloads.get(id).map(downloads::Download::payload)
            {
                self.start_save(
                    id,
                    downloads::Destination::Into {
                        directory,
                        filename: filename.clone(),
                    },
                    bytes,
                );
            }

            let tab = &mut self.tabs[index];
            tab.system = Some(SystemPage::Downloads);
            tab.url = SystemPage::Downloads.url().to_owned();
            tab.title = SystemPage::Downloads.label().to_owned();
            tab.error = None;
            tab.page = None;
            tab.pending = None;
            if record {
                self.record_history(index, &previous_url);
            }
            if index == self.active {
                self.sync_address();
                self.activate_surface(SURFACE_SYSTEM);
            }
            tracing::info!(%filename, %final_url, size, "attachment downloaded");
            return true;
        }

        // The scripts this document names are asked for here, before it is
        // parsed — see [`prefetch_scripts`]. The parse does *not* wait for
        // them: a browser shows what it has parsed and stops only at the script
        // it has actually reached. What arrives before the parse reaches it
        // runs in place; the rest is run after, which is the `defer` shape and
        // is what these still are until the parser can suspend.
        if otlyra_net::sniff(
            loaded.content_type.as_deref(),
            loaded.nosniff,
            &loaded.bytes,
        )
        .is_document()
        {
            self.prefetch_scripts(index, &loaded);
        }

        self.build_document(index, loaded, otlyra_html::ExternalSources::new())
    }

    /// Ask for every script a document's bytes name, before parsing them.
    ///
    /// This is the preload scan, and it is the whole of what a browser does
    /// ahead of its parser: read the raw bytes for what will be needed and put
    /// the requests on the wire now, so that by the time the parser reaches a
    /// `<script src>` its bytes are already coming. It does not decide when
    /// anything runs.
    fn prefetch_scripts(&mut self, index: usize, loaded: &crate::fetcher::Loaded) -> bool {
        let srcs = otlyra_html::prescan_scripts(&loaded.bytes);
        report_limit(srcs.len(), SCRIPT_LIMIT, "scripts");
        let mut outstanding: HashMap<u64, Vec<PendingResource>> = HashMap::new();
        self.request_subresources(
            &mut outstanding,
            &loaded.final_url,
            srcs.into_iter().take(SCRIPT_LIMIT).map(|src| {
                (
                    src.clone(),
                    PendingResource::ScriptSource(src),
                    ResourceKind::Script,
                )
            }),
        );
        if outstanding.is_empty() {
            return false;
        }
        let Some(pending) = self.tabs[index].pending.as_mut() else {
            return false;
        };
        tracing::debug!(
            target: "page.script",
            requests = outstanding.len(),
            url = %loaded.final_url,
            "scripts asked for ahead of the parse"
        );
        pending.outstanding = outstanding;
        true
    }

    /// Parse a document that has everything it needed to be parsed with, and put
    /// it in the tab.
    ///
    /// `sources` is what the scripts it links to turned out to hold, keyed by the
    /// `src` that named them; the parse runs each of them at its own point.
    fn build_document(
        &mut self,
        index: usize,
        loaded: crate::fetcher::Loaded,
        sources: otlyra_html::ExternalSources,
    ) -> bool {
        if let Some(pending) = self.tabs[index].pending.as_mut() {
            pending.document_arrived = true;
        }
        let interface = self.interface;

        // What the response is, from what the server said and from the bytes: a
        // picture is shown as one and text is shown as text, rather than everything
        // being fed to the HTML parser and rendering as whatever that makes of it.
        let sniffed = otlyra_net::sniff(
            loaded.content_type.as_deref(),
            loaded.nosniff,
            &loaded.bytes,
        );
        let final_url = loaded.final_url;
        tracing::debug!(kind = sniffed.essence(), url = %final_url, "response sniffed");
        // The page's own code runs at the parser's script points, in an isolate
        // of this document's own with no capabilities. The parse begins here and
        // may not end here: a `<script src>` whose bytes have not arrived stops
        // it, and what has been parsed so far goes on screen while the rest of
        // the document waits for that script — which is what a browser does and
        // is the whole reason a page appears before its bundles have landed.
        let mut parser = None;
        let parsed = match &sniffed {
            kind if kind.is_document() => {
                let started = otlyra_html::start_parse(
                    &loaded.bytes,
                    loaded.charset.as_deref(),
                    Some(Box::new(otlyra_script::PageScripts::new(final_url.clone()))),
                    sources,
                );
                parser = Some(started.parser);
                started.document
            }
            otlyra_net::Sniffed::Image(_) => {
                otlyra_html::parse(image_document(&final_url).as_bytes(), Some("utf-8")).document
            }
            _ => {
                let text = decode_text(&loaded.bytes, loaded.charset.as_deref());
                otlyra_html::parse(text_document(&text).as_bytes(), Some("utf-8")).document
            }
        };
        // Nothing that is not a document has a parser, and nothing without a
        // parser can be blocked; both of those finish here.
        let mut page_scripts: Option<Box<dyn otlyra_html::ScriptRunner>> = None;
        let mut parsed = parsed;
        let blocked = match parser.as_mut() {
            Some(parser) => parser.blocked_on().is_some(),
            None => false,
        };
        let deferred_scripts = match parser.as_mut() {
            Some(parser) => parser.deferred_scripts().to_vec(),
            None => Vec::new(),
        };
        if !blocked && let Some(parser) = parser.take() {
            page_scripts = parser.finish(&mut parsed);
        }
        let parsed = ParsedSoFar {
            document: parsed,
            deferred_scripts,
        };

        // What the page asks for, decided here and fetched on the other thread.
        let mut outstanding: HashMap<u64, Vec<PendingResource>> = HashMap::new();
        // What the preload scan already put on the wire, taken back so this
        // load's own table does not overwrite it.
        let preloaded: HashMap<u64, Vec<PendingResource>> = self.tabs[index]
            .pending
            .as_mut()
            .map(|pending| std::mem::take(&mut pending.outstanding))
            .unwrap_or_default();
        let mut preloaded = preloaded;
        // Pictures that were decoded for an earlier page and are still here.
        let mut ready = Images::default();
        let links = otlyra_css::cascade::stylesheet_links(&parsed.document);
        self.request_subresources(
            &mut outstanding,
            &final_url,
            links.iter().take(STYLESHEET_LIMIT).map(|link| {
                (
                    link.href.clone(),
                    PendingResource::Stylesheet(link.node),
                    ResourceKind::Stylesheet,
                )
            }),
        );
        // Which of the pictures an element offers is a question about the
        // window: how wide it is and how many device pixels it has to a CSS
        // pixel. Asked here, before the fetch, because a browser fetches the
        // one it chose rather than all of them.
        let pictures = otlyra_layout::image_sources(&parsed.document, self.picture_viewport());
        let wanted: Vec<_> = pictures
            .iter()
            .take(IMAGE_LIMIT)
            .filter(|source| {
                // Already decoded: no request, no decode, straight into the page.
                let Some(url) = Self::subresource_url(&final_url, &source.src) else {
                    return true;
                };
                match self.images.get(&url) {
                    Some(image) => {
                        ready.insert(
                            source.node,
                            otlyra_layout::Picture {
                                data: image,
                                density: source.density,
                            },
                        );
                        false
                    }
                    None => true,
                }
            })
            .map(|source| {
                (
                    source.src.clone(),
                    PendingResource::Image(source.node, source.src.clone(), source.density),
                    ResourceKind::Image,
                )
            })
            .collect();
        self.request_subresources(&mut outstanding, &final_url, wanted.into_iter());

        // The scripts the parse did not run where they stood, because their
        // bytes were not in yet. They run after it, in document order, which is
        // `defer` — late, and the thing the parser learning to suspend will fix.
        let scripts: Vec<(NodeId, String)> = parsed
            .deferred_scripts
            .iter()
            .filter_map(|node| {
                let src = parsed
                    .document
                    .get(*node)?
                    .element()?
                    .attr("src")?
                    .trim()
                    .to_owned();
                (!src.is_empty()).then_some((*node, src))
            })
            .take(SCRIPT_LIMIT)
            .collect();
        let script_order: Vec<NodeId> = scripts.iter().map(|(node, _)| *node).collect();
        // The preload scan already asked for most of these, and those requests
        // are in flight now. Asking again would be a second request for a file
        // already coming: what the scan started is adopted here instead, by
        // naming the node its bytes belong to.
        let mut wanted = Vec::new();
        // The script the parse is *stopped* at comes first and is not one of
        // the deferred ones: nothing after it exists yet, so it is the only
        // thing standing between this page and the rest of itself.
        let stopped_at = parser
            .as_ref()
            .and_then(otlyra_html::HtmlParser::blocked_on)
            .and_then(|node| {
                let src = parsed
                    .document
                    .get(node)?
                    .element()?
                    .attr("src")?
                    .trim()
                    .to_owned();
                (!src.is_empty()).then_some((node, src))
            });
        for (node, src) in stopped_at.into_iter().chain(scripts) {
            let mut adopted = false;
            for resource in preloaded.values_mut() {
                if resource
                    .iter()
                    .any(|held| matches!(held, PendingResource::ScriptSource(held) if *held == src))
                {
                    resource.push(PendingResource::Script(node));
                    adopted = true;
                    break;
                }
            }
            if !adopted {
                wanted.push((src, PendingResource::Script(node), ResourceKind::Script));
            }
        }
        outstanding.extend(preloaded);
        self.request_subresources(&mut outstanding, &final_url, wanted.into_iter());
        report_limit(links.len(), STYLESHEET_LIMIT, "stylesheets");
        report_limit(pictures.len(), IMAGE_LIMIT, "pictures");

        let tab = &mut self.tabs[index];
        tab.title = title_of(&parsed.document).unwrap_or_else(|| final_url.clone());
        tab.url = final_url.clone();
        tab.page = Some(PageScene::new(parsed.document));
        if !interface && let Some(page) = tab.page.as_mut() {
            page.hide_scrollbars();
        }
        if index == self.active {
            self.sync_address();
        }

        let Some(pending) = self.tabs[index].pending.as_mut() else {
            return true;
        };
        pending.outstanding = outstanding;
        pending.images.extend(ready);
        pending.script_order = script_order;
        pending.runner = page_scripts;
        // A parse that is stopped keeps its parser here. The page above is
        // already on screen; this is what carries on tokenizing into it when
        // the script it is waiting for arrives.
        pending.parse = parser;
        let record = pending.record;
        let previous = pending.previous_url.clone();

        if record {
            self.record_history(index, &previous);
        }
        if self.tabs[index]
            .pending
            .as_ref()
            .is_some_and(|pending| pending.outstanding.is_empty())
        {
            self.finish_load(index);
        }
        true
    }

    /// Ask for a page's subresources, recording which nodes each answer feeds.
    fn request_subresources(
        &mut self,
        outstanding: &mut HashMap<u64, Vec<PendingResource>>,
        base: &str,
        wanted: impl Iterator<Item = (String, PendingResource, ResourceKind)>,
    ) {
        // One request per address: a page that names the same picture in ten places
        // is asking for it once.
        let mut asked: HashMap<String, u64> = HashMap::new();
        for (href, resource, kind) in wanted {
            // A preference the browser reads where the behaviour lives. Refusing
            // here rather than dropping the bytes later is what makes it mean
            // anything: a picture that is fetched and then not shown has already
            // cost the reader their bandwidth and told the server they were here.
            if kind == ResourceKind::Image && !self.settings.settings.load_images {
                continue;
            }
            let Some(url) = Self::subresource_url(base, &href) else {
                continue;
            };
            let id = *asked
                .entry(url.clone())
                .or_insert_with(|| self.fetcher.request(&url, kind));
            outstanding.entry(id).or_default().push(resource);
        }
    }

    /// A stylesheet or a picture arrived.
    fn receive_subresource(&mut self, index: usize, fetched: Fetched) {
        let Some(pending) = self.tabs[index].pending.as_mut() else {
            return;
        };
        let Some(wanted) = pending.outstanding.remove(&fetched.id) else {
            return;
        };
        // Whether a script the parse might be waiting for has landed.
        let mut resumed = false;

        match fetched.result {
            Ok(loaded) => {
                // Decoded once, however many elements asked for it.
                let decoded = wanted
                    .iter()
                    .any(|resource| matches!(resource, PendingResource::Image(..)))
                    .then(|| {
                        otlyra_gfx::decode_image(&loaded.bytes)
                            .inspect_err(
                                |error| tracing::warn!(url = %fetched.url, %error, "image failed to decode"),
                            )
                            .ok()
                    })
                    .flatten();

                if let Some(image) = decoded.clone() {
                    self.images.insert(fetched.url.clone(), image);
                }

                for resource in wanted {
                    match resource {
                        PendingResource::Stylesheet(node) => {
                            let source = decode_text(&loaded.bytes, loaded.charset.as_deref());
                            pending.sheets.insert(node, source);
                        }
                        PendingResource::Script(node) => {
                            let source = decode_text(&loaded.bytes, loaded.charset.as_deref());
                            pending.scripts.insert(node, source);
                            resumed = true;
                        }
                        PendingResource::ScriptSource(src) => {
                            // The preload scan's own entry. It answers a node
                            // only once the parse has run and told us which
                            // node that is — see `build_document`, which adds
                            // a `Script(node)` beside this one. Before then
                            // there is nothing to give the bytes to, and after
                            // then this entry has nothing left to do.
                            tracing::trace!(target: "page.script", %src, "a preloaded script arrived");
                        }
                        PendingResource::Image(node, src, density) => match decoded.as_ref() {
                            Some(image) => {
                                pending.images.insert(
                                    node,
                                    otlyra_layout::Picture {
                                        data: image.clone(),
                                        density,
                                    },
                                );
                                pending.picture_sources.insert(node, (src, density));
                            }
                            None => {
                                tracing::warn!(url = %fetched.url, "image failed to decode");
                            }
                        },
                    }
                }
            }
            Err(error) => {
                tracing::warn!(url = %fetched.url, %error, "subresource failed to load");
            }
        }

        if resumed {
            self.resume_parse(index);
        }

        if self.tabs[index]
            .pending
            .as_ref()
            .is_some_and(|pending| pending.outstanding.is_empty())
        {
            self.finish_load(index);
        }
    }

    /// Carry on parsing a document that was stopped at a script, now that the
    /// script has arrived.
    ///
    /// The tree is the tab's and the parser borrows it back, so the page the
    /// reader is looking at grows rather than being replaced. It may stop again
    /// at the next script — a page with five bundles in its head stops five
    /// times — and each stop is another piece of the document on screen.
    fn resume_parse(&mut self, index: usize) {
        loop {
            let Some(pending) = self.tabs[index].pending.as_mut() else {
                return;
            };
            let Some(parser) = pending.parse.as_mut() else {
                return;
            };
            let Some(blocked) = parser.blocked_on() else {
                return;
            };
            // The bytes, or the knowledge that they will not come: a fetch that
            // failed must not stop the page for ever.
            let source = pending.scripts.get(&blocked).cloned();
            let Some(source) = source else {
                let failed = !pending.outstanding.values().flatten().any(
                    |resource| matches!(resource, PendingResource::Script(node) if *node == blocked),
                );
                if !failed {
                    return;
                }
                tracing::debug!(target: "page.script", "the script the parse waited for never came");
                let mut parser = pending.parse.take().expect("just borrowed");
                self.step_parse(index, |document| parser.skip_script(document));
                self.after_parse_step(index, parser);
                continue;
            };

            let mut parser = pending.parse.take().expect("just borrowed");
            self.step_parse(index, |document| parser.supply_script(document, &source));
            self.after_parse_step(index, parser);
        }
    }

    /// Run one step of a stopped parse over the tab's own tree.
    fn step_parse(&mut self, index: usize, run: impl FnOnce(&mut otlyra_dom::Document)) {
        if let Some(page) = self.tabs[index].page.as_mut() {
            page.with_document(run);
            // The tree is longer than it was, so everything style and layout
            // made of it is out of date.
            page.document_changed();
        }
    }

    /// Put the parser back, or finish the parse if it has nothing left to wait
    /// for.
    fn after_parse_step(&mut self, index: usize, parser: otlyra_html::HtmlParser) {
        if parser.blocked_on().is_some() {
            // Stopped again, at the next script. Ask for that one.
            let src = self.tabs[index].page.as_ref().and_then(|page| {
                let node = parser.blocked_on()?;
                let src = page
                    .document()
                    .get(node)?
                    .element()?
                    .attr("src")?
                    .trim()
                    .to_owned();
                (!src.is_empty()).then_some((node, src))
            });
            if let Some((node, src)) = src {
                let base = self.tabs[index].url.clone();
                let mut outstanding = HashMap::new();
                self.request_subresources(
                    &mut outstanding,
                    &base,
                    std::iter::once((src, PendingResource::Script(node), ResourceKind::Script)),
                );
                if let Some(pending) = self.tabs[index].pending.as_mut() {
                    for (id, resources) in outstanding {
                        pending.outstanding.entry(id).or_default().extend(resources);
                    }
                }
            }
            if let Some(pending) = self.tabs[index].pending.as_mut() {
                pending.parse = Some(parser);
            }
            return;
        }

        // Nothing left to wait for: the last byte is parsed and the load events
        // are due.
        let mut runner = None;
        if let Some(page) = self.tabs[index].page.as_mut() {
            page.with_document(|document| runner = parser.finish(document));
            page.document_changed();
        }
        if let Some(pending) = self.tabs[index].pending.as_mut()
            && let Some(runner) = runner
        {
            pending.runner = Some(runner);
        }
    }

    /// Whether a tab is still waiting for a stylesheet it cannot be drawn without.
    ///
    /// A `<link rel=stylesheet>` in the head is render-blocking, and that is not
    /// a detail: a document painted before its stylesheet arrives is painted in
    /// the wrong fonts, at the wrong widths, in the wrong colours, and then
    /// jumps. Every browser holds the frame instead, and what a reader sees on a
    /// slow load is the last page or nothing — never the author's markup with
    /// none of the author's design on it.
    ///
    /// Pictures are not on this list. They are not render-blocking anywhere, and
    /// a page held back for a photograph is a page nobody can start reading.
    pub(super) fn blocked_on_style(&self, index: usize) -> bool {
        let Some(pending) = self.tabs[index].pending.as_ref() else {
            return false;
        };
        let document = self.tabs[index].page.as_ref().map(PageScene::document);
        let viewport = self.picture_viewport();
        pending
            .outstanding
            .values()
            .flatten()
            .any(|resource| match resource {
                PendingResource::Stylesheet(node) => {
                    // A sheet written for another medium blocks nothing: a
                    // print-only one holds the screen for a page it will never
                    // style. What it says it is for is asked of the same matcher
                    // the cascade uses, so the two cannot come to disagree.
                    document.is_none_or(|document| {
                        crate::media_of_link(document, *node).is_none_or(|media| {
                            otlyra_css::cascade::media_condition_matches(&media, viewport)
                        })
                    })
                }
                PendingResource::Image(..)
                | PendingResource::Script(..)
                | PendingResource::ScriptSource(..) => false,
            })
    }

    /// Everything the page asked for has arrived or failed: build it for real.
    fn finish_load(&mut self, index: usize) {
        // A new page names its own backgrounds; what the last one asked for is not
        // an answer for this one.
        self.background_requests.clear();

        let Some(pending) = self.tabs[index].pending.take() else {
            return;
        };
        let scroll = pending.restore_scroll;
        let mut pending = pending;
        let tab = &mut self.tabs[index];

        // The scripts the page linked to, in the order it named them, then the
        // load events that were waiting for them. This happens before the scene
        // is rebuilt, so whatever they did to the document is in the first frame
        // the reader sees rather than in a second one after a flash.
        let mut document = tab.page.take().map(PageScene::into_document);
        if let (Some(document), Some(runner)) = (document.as_mut(), pending.runner.as_mut()) {
            for node in &pending.script_order {
                if let Some(source) = pending.scripts.get(node) {
                    runner.run_external(source, *node, document);
                }
            }
            runner.document_finished(document, false);
        }
        // The page's own deferred work, up to a bounded budget. A page that
        // paints from a `setTimeout(…, 0)` — which is most of what a framework
        // does after hydrating — has done it by the time the first frame is
        // built, rather than in a second frame after a flash. What it schedules
        // beyond the budget waits for the loop, which is where a timer belongs.
        if let (Some(document), Some(runner)) = (document.as_mut(), pending.runner.as_mut()) {
            let ran = runner.settle(document, SETTLE_BUDGET);
            if ran > 0 {
                tracing::debug!(target: "page.script", ran, "the page settled");
            }
        }
        // The isolate stays with the tab: its timers are still coming due and
        // its listeners are still waiting.
        tab.scripts = pending.runner.take();

        if let Some(document) = document {
            tab.page = Some(PageScene::with_resources(
                document,
                pending.sheets,
                pending.images,
                pending.picture_sources,
            ));
        }
        if let Some(page) = tab.page.as_mut() {
            page.set_scroll(scroll);
        }

        self.follow_script_navigation(index);
    }

    /// Go where the page's script said to go.
    ///
    /// A redirector page is a page whose whole content is an instruction to be
    /// somewhere else — a sign-in bounce, a shortener, an old address. Script
    /// asks; the browser decides, and decides here rather than inside the
    /// isolate, because the isolate is holding the document a navigation
    /// destroys.
    fn follow_script_navigation(&mut self, index: usize) {
        // This tab's, not the thread's. A navigation asked for by one tab was
        // once answerable by whichever tab was being pumped when it was noticed.
        let Some(request) = self
            .tabs
            .get_mut(index)
            .and_then(|tab| tab.scripts.as_mut())
            .and_then(|scripts| scripts.take_navigation())
        else {
            return;
        };
        if index != self.active {
            // A background tab redirecting itself is a real thing and not this
            // change: everything below aims at the tab the reader is looking at.
            tracing::debug!(?request, "a background tab's script asked to navigate");
            return;
        }
        if self.script_hops >= SCRIPT_HOP_LIMIT {
            tracing::warn!(
                hops = self.script_hops,
                "page script is navigating in a loop"
            );
            return;
        }
        self.script_hops += 1;

        let here = self.tabs[index].url.clone();
        match request {
            otlyra_script::dom::Navigation::Reload => {
                self.start_load(&here, false, false, 0.0);
            }
            otlyra_script::dom::Navigation::Url { href, replace } => {
                let target = otlyra_net::url::resolve(&here, &href).unwrap_or(href);
                if target == here {
                    // `location.href = location.href` is a reload, and a page
                    // that does it on every load is the loop above.
                    tracing::debug!(url = %target, "script navigated to where it already is");
                }
                self.remember_scroll();
                // A replacing navigation is the same fetch without the history
                // entry, which is exactly what a redirector wants: back should
                // go past it, not to it.
                self.start_load(&target, false, !replace, 0.0);
            }
            otlyra_script::dom::Navigation::Submit { form } => {
                let staged = self.tabs[index]
                    .page
                    .as_mut()
                    .is_some_and(|page| page.submit_from_script(form));
                if staged {
                    self.follow_submission();
                }
            }
        }
    }

    /// The address a subresource is actually fetched from, or `None` if the page
    /// may not reach it.
    ///
    /// A document fetched over the network may not reach a `file:` URL, the same
    /// rule that governs where it may navigate: a subresource is a request the page
    /// chose to make, and a page from the internet reading the disk is the failure
    /// that rule exists to prevent.
    pub(super) fn subresource_url(base: &str, href: &str) -> Option<String> {
        let url = otlyra_net::resolve(base, href)?;
        if let Ok(target) = otlyra_net::normalize(&url)
            && !otlyra_net::may_navigate(Some(base), &target)
        {
            tracing::warn!(%url, %base, "subresource refused by scheme policy");
            return None;
        }
        Some(url)
    }
}
