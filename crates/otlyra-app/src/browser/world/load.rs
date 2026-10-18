//! A load, from the request to the last thing the page asked for.
//!
//! The document is asked for, sniffed, parsed — stopping at any script it has to
//! wait for — and shown, and then its stylesheets, pictures and scripts arrive and
//! the page is built for real. It is the longest path in the browser and the only
//! one that holds a `PendingLoad`, so it is kept whole, from the first byte to the
//! last.

use std::collections::{HashMap, HashSet};

use otlyra_css::cascade::{FetchedSheet, StyleSources};
use otlyra_dom::NodeId;
use otlyra_layout::Images;
use url::Url;

use crate::downloads;
use crate::fetcher::{Body, Fetched, ResourceKind};
use crate::page::{PageScene, title_of};

use super::super::TabId;
use super::{PageWorld, ScriptNavigation, WorldEvent};

/// A load in flight, and everything it is still waiting for.
pub(super) struct PendingLoad {
    pub(super) started: std::time::Instant,
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
    /// The base the page's style resolves against, and the stylesheets fetched
    /// for it so far.
    sources: std::rc::Rc<std::cell::RefCell<StyleSources>>,
    /// What styles the document for script, over `sources`: shared with the
    /// page once it is shown, which lays the document out for script with it.
    script_styler: Option<std::rc::Rc<otlyra_css::cascade::ScriptStyler>>,
    /// Every sheet an `@import` has asked for, by address: a sheet imported
    /// twice, or by a sheet it imports itself, is fetched once.
    imports_asked: HashSet<Url>,
    /// How many stylesheets have been asked for, linked and imported together.
    sheets_asked: usize,
    /// The `<link rel=stylesheet>` elements already asked for. A parse stopped
    /// at a script shows the document a piece at a time, and each piece may
    /// link a sheet the last one did not have.
    links_asked: HashSet<NodeId>,
    /// The same for the elements that show a picture.
    pictures_asked: HashSet<NodeId>,
    images: Images,
    /// Which file each of those pictures came from, and at what density.
    picture_sources: HashMap<NodeId, (String, f32)>,
    /// What each outstanding request will feed once it arrives.
    pub(super) outstanding: HashMap<u64, Vec<PendingResource>>,
    /// The parse, when it is stopped part way at a script it is waiting for.
    ///
    /// A page whose parser is stopped is already on screen: everything before
    /// the script is in the tree the tab is showing. What is here is the state
    /// that carries on tokenizing once the script's bytes arrive.
    pub(super) parse: Option<otlyra_html::HtmlParser>,
    /// The `<script src>` elements, in the order the document names them.
    script_order: Vec<NodeId>,
    /// The source of each of those that has arrived.
    scripts: HashMap<NodeId, String>,
    /// The page's script world, kept alive from the parse until the external
    /// scripts have run in it.
    pub(super) runner: Option<Box<dyn otlyra_html::ScriptRunner>>,
    /// Module addresses already asked for during this load — by the preload
    /// scan, or for the page's runner — so none is asked for twice.
    pub(super) modules_asked: HashSet<String>,
    /// When the load passed each of its stages, for the `page.timing` line.
    timing: LoadTiming,
}

/// When a load passed each of its stages, and how long the work in the last
/// of them took: what says whether a slow page waited on the network, on its
/// parse, or on its scripts.
#[derive(Debug, Default)]
struct LoadTiming {
    /// The document's bytes arrived.
    arrived: Option<std::time::Instant>,
    /// The document was parsed as far as it could be and first shown.
    built: Option<std::time::Instant>,
    /// Running what the parse ran on the way: the parse itself and the scripts
    /// it ran in place.
    build: std::time::Duration,
}

impl PendingLoad {
    pub(super) fn set_viewport(
        &mut self,
        document: &mut otlyra_dom::Document,
        viewport: otlyra_css::cascade::Viewport,
    ) -> bool {
        let (width, height, scale) = (
            f64::from(viewport.width),
            f64::from(viewport.height),
            f64::from(viewport.scale),
        );
        if let Some(parser) = self.parse.as_mut() {
            parser.set_viewport(document, width, height, scale)
        } else if let Some(runner) = self.runner.as_mut() {
            runner.set_viewport(document, width, height, scale);
            runner.take_mutated()
        } else {
            false
        }
    }
    /// Visibility belongs to the document even before its load is complete.
    pub(super) fn set_hidden(&mut self, document: &mut otlyra_dom::Document, hidden: bool) -> bool {
        if let Some(parser) = self.parse.as_mut() {
            parser.set_hidden(document, hidden)
        } else if let Some(runner) = self.runner.as_mut() {
            runner.set_hidden(document, hidden);
            runner.take_mutated()
        } else {
            false
        }
    }
}

/// What a subresource is for once it lands.
pub(super) enum PendingResource {
    /// The `<link>` whose stylesheet this is.
    Stylesheet(NodeId),
    /// A sheet an `@import` asked for.
    Import(Import),
    /// The `<img>` whose picture this is, the address it settled on as the
    /// markup spells it, and that candidate's density — which is what the file's
    /// own size is divided by.
    Image(NodeId, String, f32),
    /// The `<script src>` whose source this is.
    Script(NodeId),
    /// A script asked for before the parse, by the address it was fetched from.
    ///
    /// There is no node to hang it on yet — the document it is in has not been
    /// parsed, which is the point. The address is the `src` resolved against the
    /// document's own: the scan reads bytes and cannot know of a base element,
    /// so it is the address, not the text, that the parse's scripts are matched
    /// against.
    ScriptSource(String),
    /// An ES module, by its address in the page's module map.
    Module(String),
}

/// A sheet an `@import` asked for (CSS Cascade 5 §2).
pub(super) struct Import {
    /// The address the rule named, resolved against the sheet it is in.
    url: Url,
    /// How many imports deep it is: one for a rule in a linked sheet or a
    /// `<style>`.
    depth: usize,
    /// The `<link>` or `<style>` whose sheet the imports began in. An import
    /// holds the first frame exactly when that element's sheet does.
    owner: NodeId,
}

impl Import {
    /// The imports `sheet` asks for, one level below `depth`.
    fn all_in(sheet: &FetchedSheet, depth: usize, owner: NodeId) -> impl Iterator<Item = Self> {
        otlyra_css::cascade::imports_in(&sheet.text, &sheet.url)
            .into_iter()
            .map(move |url| Self {
                url,
                depth: depth + 1,
                owner,
            })
    }
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

/// How many stylesheets one document may pull in, linked and imported together.
///
/// A limit rather than none: every one of these is a synchronous fetch on the way
/// to the first frame, and a document that asks for hundreds is either generated
/// or hostile.
const STYLESHEET_LIMIT: usize = 256;

/// How deep `@import`s may nest: a sheet imported by a sheet imported by a
/// sheet, and one more. The specification sets no limit; this is one against
/// a chain that is generated rather than written.
const IMPORT_DEPTH_LIMIT: usize = 4;

/// How many pictures one document may pull in, for the same reason.
pub(super) const IMAGE_LIMIT: usize = 256;

/// How many scripts one page may link to.
///
/// A page with more than this many is not a page we refuse; it is a page whose
/// hundredth script we decline to fetch, the way the stylesheet and picture
/// limits work.
const SCRIPT_LIMIT: usize = 256;

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

impl PageWorld {
    /// Ask for `url` for `tab`, and leave it waiting.
    ///
    /// Nothing here waits: the request goes to the fetch thread and the answer
    /// arrives as a finished fetch like any other. What the tab showed stays on
    /// screen until the document that replaces it arrives. `previous_url` is
    /// where the tab was, `record` whether arriving should earn a history entry,
    /// and `restore_scroll` where the reader should be put once it has.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::browser) fn start(
        &mut self,
        tab: TabId,
        url: &str,
        body: Option<Body>,
        cache: otlyra_net::CacheMode,
        previous_url: String,
        record: bool,
        restore_scroll: f32,
    ) {
        let started = std::time::Instant::now();
        // A load this one replaces is one nobody will read.
        self.cancel_load(tab);
        // A form's post comes from the page that holds the form, which is what
        // keeps a cross-site post from carrying the reader's `SameSite`
        // cookies. A plain navigation is taken as the reader's own.
        let initiator = body
            .is_some()
            .then(|| previous_url.clone())
            .filter(|page| !page.is_empty());
        // A frame's document is a request its page made, under its page's rules.
        let id = match self.frame_parent_url(tab) {
            Some(parent) => self.fetcher.load_frame(tab.0, url, cache, parent),
            None => self.fetcher.navigate(tab.0, url, body, cache, initiator),
        };
        let slot = self.slots.entry(tab).or_default();
        slot.viewport_zoom = Some(self.environment.page_zoom);
        slot.pending = Some(PendingLoad {
            started,
            document: id,
            document_arrived: false,
            previous_url,
            record,
            restore_scroll,
            sources: std::rc::Rc::new(std::cell::RefCell::new(StyleSources::default())),
            script_styler: None,
            imports_asked: HashSet::new(),
            sheets_asked: 0,
            links_asked: HashSet::new(),
            pictures_asked: HashSet::new(),
            images: Images::default(),
            picture_sources: HashMap::new(),
            outstanding: HashMap::new(),
            parse: None,
            script_order: Vec::new(),
            scripts: HashMap::new(),
            runner: None,
            modules_asked: HashSet::new(),
            timing: LoadTiming::default(),
        });
        slot.modules_fetched = 0;
    }

    /// The load in flight for `tab`, if there is one.
    pub(super) fn pending(&self, tab: TabId) -> Option<&PendingLoad> {
        self.slots.get(&tab)?.pending.as_ref()
    }

    /// The same, to change.
    pub(super) fn pending_mut(&mut self, tab: TabId) -> Option<&mut PendingLoad> {
        self.slots.get_mut(&tab)?.pending.as_mut()
    }

    /// The CSS viewport `tab`'s script sees: the window, at the tab's zoom.
    fn script_viewport(&self, tab: TabId) -> otlyra_css::cascade::Viewport {
        if let Some(viewport) = self.frame_viewport(tab) {
            return viewport;
        }
        let mut viewport = self.environment.viewport;
        let zoom = self
            .slots
            .get(&tab)
            .and_then(|slot| slot.viewport_zoom)
            .unwrap_or(1.0);
        viewport.width /= zoom;
        viewport.height /= zoom;
        viewport.scale *= zoom;
        viewport
    }

    /// The page itself arrived.
    ///
    /// The document is shown straight away, before its stylesheets and pictures
    /// have been asked for: a page that is readable now and styled a moment later
    /// beats a blank window for the length of the slowest thing it links to.
    pub(super) fn receive_document(&mut self, tab: TabId, fetched: Fetched) -> bool {
        let loaded = match fetched.result {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(%error, "navigation failed");
                self.set_page(tab, None);
                self.events.push(WorldEvent::Failed { tab, error });
                return true;
            }
        };

        // A document that refuses to be shown in this frame is never parsed:
        // its scripts do not run, and its element shows nothing.
        if let Some(why) = self.frame_refused(tab, &loaded) {
            tracing::info!(target: "page.frames", url = %loaded.final_url, %why, "a frame was refused its document");
            self.set_page(tab, None);
            return true;
        }

        // An attachment is a completed download, not a document with unusual
        // bytes. The payload goes to the window, which keeps the downloads, and
        // is not sent through MIME sniffing and the HTML parser.
        if let Some(filename) =
            downloads::attachment_filename(&loaded.response_headers, &loaded.final_url)
        {
            let recorded = self
                .pending(tab)
                .and_then(|pending| pending.record.then(|| pending.previous_url.clone()));
            self.set_page(tab, None);
            self.events.push(WorldEvent::Attachment {
                tab,
                filename,
                loaded,
                recorded,
            });
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
            self.prefetch_scripts(tab, &loaded);
        }

        let arrived = std::time::Instant::now();
        if let Some(pending) = self.pending_mut(tab) {
            pending.timing.arrived = Some(arrived);
        }
        let built = self.build_document(tab, loaded, otlyra_html::ExternalSources::new());
        if let Some(pending) = self.pending_mut(tab) {
            pending.timing.built = Some(std::time::Instant::now());
            pending.timing.build = arrived.elapsed();
        }
        built
    }

    /// Ask for every script a document's bytes name, before parsing them.
    ///
    /// This is the preload scan, and it is the whole of what a browser does
    /// ahead of its parser: read the raw bytes for what will be needed and put
    /// the requests on the wire now, so that by the time the parser reaches a
    /// `<script src>` its bytes are already coming. It does not decide when
    /// anything runs.
    fn prefetch_scripts(&mut self, tab: TabId, loaded: &crate::fetcher::Loaded) -> bool {
        let srcs = otlyra_html::prescan_scripts(&loaded.bytes);
        report_limit(srcs.len(), SCRIPT_LIMIT, "scripts");
        // The scan reads bytes, not a tree, and sees no base element: what it
        // finds resolves against the document's own address.
        let here = document_url(&loaded.final_url);
        let urls: Vec<String> = srcs
            .iter()
            .take(SCRIPT_LIMIT)
            .filter_map(|src| Self::subresource_url(here.as_str(), &here, src))
            .collect();
        // Module scripts never stop the parse, but each one's graph is
        // fetched before it runs and its entry is the first link of it.
        let modules: Vec<String> = otlyra_html::prescan_module_scripts(&loaded.bytes)
            .iter()
            .take(super::modules::MODULE_LIMIT)
            .filter_map(|src| Self::subresource_url(here.as_str(), &here, src))
            .collect();
        let mut outstanding: HashMap<u64, Vec<PendingResource>> = HashMap::new();
        self.request_subresources(
            tab,
            &mut outstanding,
            here.as_str(),
            &here,
            urls.into_iter()
                .map(|url| {
                    (
                        url.clone(),
                        PendingResource::ScriptSource(url),
                        ResourceKind::Script,
                    )
                })
                .chain(modules.iter().map(|url| {
                    (
                        url.clone(),
                        PendingResource::Module(url.clone()),
                        ResourceKind::Module,
                    )
                })),
        );
        if outstanding.is_empty() {
            return false;
        }
        let Some(pending) = self.pending_mut(tab) else {
            return false;
        };
        tracing::debug!(
            target: "page.script",
            requests = outstanding.len(),
            url = %loaded.final_url,
            "scripts asked for ahead of the parse"
        );
        pending.outstanding = outstanding;
        let asked = modules.len();
        pending.modules_asked.extend(modules);
        if let Some(slot) = self.slots.get_mut(&tab) {
            slot.modules_fetched += asked;
        }
        true
    }

    /// Parse a document that has everything it needed to be parsed with, and put
    /// it in the tab.
    ///
    /// `sources` is what the scripts it links to turned out to hold, keyed by the
    /// `src` that named them; the parse runs each of them at its own point.
    fn build_document(
        &mut self,
        tab: TabId,
        loaded: crate::fetcher::Loaded,
        sources: otlyra_html::ExternalSources,
    ) -> bool {
        if let Some(pending) = self.pending_mut(tab) {
            pending.document_arrived = true;
        }
        let interface = self.environment.interface;

        // What the response is, from what the server said and from the bytes: a
        // picture is shown as one and text is shown as text, rather than everything
        // being fed to the HTML parser and rendering as whatever that makes of it.
        let sniffed = otlyra_net::sniff(
            loaded.content_type.as_deref(),
            loaded.nosniff,
            &loaded.bytes,
        );
        let final_url = loaded.final_url;
        // What this document asks for from now on is caused by it.
        self.fetcher.set_initiator(tab.0, Some(final_url.clone()));
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
                let mut scripts = Box::new(otlyra_script::PageScripts::new(final_url.clone()));
                if let Some(jar) = &self.cookie_jar {
                    scripts.set_cookies(std::sync::Arc::new(crate::cookies::ScriptCookies(
                        std::sync::Arc::clone(jar),
                    )));
                }
                let session =
                    std::sync::Arc::clone(&self.slots.entry(tab).or_default().session_storage);
                scripts.set_storage(std::sync::Arc::clone(&self.local_storage), session);
                if let Some(cache) = &self.code_cache {
                    scripts.set_code_cache(std::sync::Arc::clone(cache));
                }
                // `load` waits for the document's pictures and sheets, which
                // this world fetches; it fires it in `finish_load`.
                scripts.hold_load_event();
                if let Some(pending) = self.pending(tab) {
                    scripts.set_initial_time_origin(pending.started);
                }
                scripts
                    .set_initial_hidden(self.environment.visible_tab() != Some(self.top_of(tab)));
                let viewport = self.script_viewport(tab);
                scripts.set_initial_viewport(
                    f64::from(viewport.width),
                    f64::from(viewport.height),
                    f64::from(viewport.scale),
                );
                scripts.set_initial_color_scheme(
                    viewport.color_scheme == otlyra_css::cascade::ColorScheme::Dark,
                );
                let started = otlyra_html::start_parse(
                    &loaded.bytes,
                    loaded.charset.as_deref(),
                    Some(scripts),
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
        // `getComputedStyle` after a change styles the tree there and then,
        // with the sheets this load has fetched so far — the ones a script
        // after a `<link>` sees in a browser.
        let viewport = self.script_viewport(tab);
        if let Some(pending) = self.pending_mut(tab) {
            let styler = std::rc::Rc::new(otlyra_css::cascade::ScriptStyler::new(
                std::rc::Rc::clone(&pending.sources),
                viewport,
            ));
            parsed.set_style_resolver(Some(
                std::rc::Rc::clone(&styler) as std::rc::Rc<dyn otlyra_dom::StyleResolver>
            ));
            pending.script_styler = Some(styler);
        }
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

        // The scene the reader sees while the rest arrives. Everything below is
        // asked for on the document's authority and resolved against its base
        // URL, which a base element in what has been parsed so far can move.
        let page = shown(PageScene::at(parsed, document_url(&final_url)), interface);
        let (here, base) = (page.url().to_string(), page.base_url().clone());
        let document = page.document();

        // What the page asks for, decided here and fetched on the other thread.
        let mut outstanding: HashMap<u64, Vec<PendingResource>> = HashMap::new();
        // What the preload scan already put on the wire, taken back so this
        // load's own table does not overwrite it.
        let mut preloaded: HashMap<u64, Vec<PendingResource>> = self
            .pending_mut(tab)
            .map(|pending| std::mem::take(&mut pending.outstanding))
            .unwrap_or_default();
        // Pictures that were decoded for an earlier page and are still here.
        let mut ready = Images::default();
        let links = otlyra_css::cascade::stylesheet_links(document);
        self.request_subresources(
            tab,
            &mut outstanding,
            &here,
            &base,
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
        let pictures = otlyra_layout::image_sources(document, self.environment.viewport);
        let wanted: Vec<_> = pictures
            .iter()
            .take(IMAGE_LIMIT)
            .filter(|source| {
                // Already decoded: no request, no decode, straight into the page.
                let Some(url) = Self::subresource_url(&here, &base, &source.src) else {
                    return true;
                };
                match self.images.get(&url) {
                    Some(image) => {
                        ready.insert(source.node, image.for_element(source.density));
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
        self.request_subresources(tab, &mut outstanding, &here, &base, wanted.into_iter());

        // The scripts the parse did not run where they stood, because their
        // bytes were not in yet. They run after it, in document order, which is
        // `defer` — late, and the thing the parser learning to suspend will fix.
        let scripts: Vec<(NodeId, String)> = deferred_scripts
            .iter()
            .filter_map(|node| {
                let src = document.attr(*node, "src")?.trim().to_owned();
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
                let src = document.attr(node, "src")?.trim().to_owned();
                (!src.is_empty()).then_some((node, src))
            });
        for (node, src) in stopped_at.into_iter().chain(scripts) {
            let Some(url) = Self::subresource_url(&here, &base, &src) else {
                continue;
            };
            let adopted = preloaded.values_mut().find(|held| {
                held.iter()
                    .any(|held| matches!(held, PendingResource::ScriptSource(held) if *held == url))
            });
            match adopted {
                Some(held) => held.push(PendingResource::Script(node)),
                None => wanted.push((url, PendingResource::Script(node), ResourceKind::Script)),
            }
        }
        outstanding.extend(preloaded);
        self.request_subresources(tab, &mut outstanding, &here, &base, wanted.into_iter());
        report_limit(links.len(), STYLESHEET_LIMIT, "stylesheets");
        report_limit(pictures.len(), IMAGE_LIMIT, "pictures");
        let imports: Vec<Import> = otlyra_css::cascade::style_element_imports(document, &base)
            .into_iter()
            .map(|(owner, url)| Import {
                url,
                depth: 1,
                owner,
            })
            .collect();
        let title = title_of(document);

        let title = title.unwrap_or_else(|| final_url.clone());
        let Some(slot) = self.slots.get_mut(&tab) else {
            return true;
        };
        slot.page = Some(page);
        let Some(pending) = slot.pending.as_mut() else {
            self.events.push(WorldEvent::Arrived {
                tab,
                title,
                url: final_url,
                recorded: None,
            });
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
        pending.sources.borrow_mut().base = base;
        pending.sheets_asked = links.len().min(STYLESHEET_LIMIT);
        pending.links_asked = links
            .iter()
            .take(STYLESHEET_LIMIT)
            .map(|link| link.node)
            .collect();
        pending.pictures_asked = pictures
            .iter()
            .take(IMAGE_LIMIT)
            .map(|source| source.node)
            .collect();
        let recorded = pending.record.then(|| pending.previous_url.clone());
        // The window hears of the arrival before anything the rest of the load
        // does: it is where the tab's title and history entry come from.
        self.events.push(WorldEvent::Arrived {
            tab,
            title,
            url: final_url,
            recorded,
        });
        self.request_imports(tab, imports);
        self.request_modules(tab);

        if self
            .pending(tab)
            .is_some_and(|pending| pending.outstanding.is_empty())
        {
            self.finish_load(tab);
        }
        true
    }

    /// Ask for a page's subresources, recording which nodes each answer feeds.
    ///
    /// Each address is resolved against `base` and fetched on the authority of
    /// the document at `document`.
    fn request_subresources(
        &mut self,
        tab: TabId,
        outstanding: &mut HashMap<u64, Vec<PendingResource>>,
        document: &str,
        base: &Url,
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
            if kind == ResourceKind::Image && !self.environment.load_images {
                continue;
            }
            let Some(url) = Self::subresource_url(document, base, &href) else {
                continue;
            };
            let id = *asked
                .entry(url.clone())
                .or_insert_with(|| self.fetcher.request_for(tab.0, &url, kind));
            outstanding.entry(id).or_default().push(resource);
        }
    }

    /// Ask for more on behalf of a load already under way, and add the
    /// requests to what it is waiting for.
    ///
    /// Asked by the page the load has already put in the tab, on its authority
    /// and against its base URL as it stands now.
    fn request_for_load(
        &mut self,
        tab: TabId,
        wanted: impl Iterator<Item = (String, PendingResource, ResourceKind)>,
    ) {
        let Some(page) = self.page(tab) else {
            return;
        };
        let (document, base) = (page.url().to_string(), page.base_url().clone());
        let mut outstanding = HashMap::new();
        self.request_subresources(tab, &mut outstanding, &document, &base, wanted);
        if let Some(pending) = self.pending_mut(tab) {
            for (id, resources) in outstanding {
                pending.outstanding.entry(id).or_default().extend(resources);
            }
        }
    }

    /// Ask for the sheets `@import` rules name: each once, however many rules
    /// name it, and no deeper and no more of them than the limits allow.
    fn request_imports(&mut self, tab: TabId, imports: Vec<Import>) {
        let Some(pending) = self.pending_mut(tab) else {
            return;
        };
        let mut wanted = Vec::new();
        for import in imports {
            if import.depth > IMPORT_DEPTH_LIMIT {
                tracing::warn!(url = %import.url, "an import nested past the limit is not fetched");
                continue;
            }
            if pending.imports_asked.contains(&import.url) {
                continue;
            }
            if pending.sheets_asked >= STYLESHEET_LIMIT {
                tracing::warn!(url = %import.url, "an import past the stylesheet limit is not fetched");
                continue;
            }
            pending.imports_asked.insert(import.url.clone());
            pending.sheets_asked += 1;
            wanted.push((
                import.url.to_string(),
                PendingResource::Import(import),
                ResourceKind::Stylesheet,
            ));
        }
        // Absolute already, each resolved against the sheet that named it.
        self.request_for_load(tab, wanted.into_iter());
    }

    /// A stylesheet or a picture arrived.
    pub(super) fn receive_subresource(&mut self, tab: TabId, fetched: Fetched) {
        // Through the field rather than `pending_mut`, so that the decoded
        // pictures can be kept while the load is held.
        let Some(pending) = self
            .slots
            .get_mut(&tab)
            .and_then(|slot| slot.pending.as_mut())
        else {
            return;
        };
        let Some(wanted) = pending.outstanding.remove(&fetched.id) else {
            return;
        };
        // Whether a script the parse might be waiting for has landed.
        let mut resumed = false;
        // What the sheets that landed import in turn.
        let mut imports = Vec::new();
        // What became of the pictures elements asked for, for their documents.
        let mut pictures = Vec::new();
        // Module sources, for the page's runner.
        let mut modules = Vec::new();
        // A sheet settled, which a script may have been waiting for.
        if wanted.iter().any(|resource| {
            matches!(
                resource,
                PendingResource::Stylesheet(_) | PendingResource::Import(_)
            )
        }) {
            resumed = true;
        }

        match fetched.result {
            Ok(mut loaded) => {
                // Decoded once, however many elements asked for it.
                let decoded = wanted
                    .iter()
                    .any(|resource| matches!(resource, PendingResource::Image(..)))
                    .then(|| {
                        let (status, bytes) = (loaded.status, loaded.bytes.len());
                        let content_type = loaded.content_type.clone();
                        loaded
                            .picture()
                            .inspect_err(|error| {
                                tracing::warn!(
                                    url = %fetched.url,
                                    %error,
                                    ?status,
                                    ?content_type,
                                    bytes,
                                    "image failed to decode"
                                )
                            })
                            .ok()
                    })
                    .flatten();

                if let Some(image) = decoded.clone() {
                    self.images.insert(fetched.url.clone(), image);
                }

                for resource in wanted {
                    match resource {
                        PendingResource::Stylesheet(node) => {
                            if let Some(sheet) = fetched_sheet(&loaded, &fetched.url) {
                                imports.extend(Import::all_in(&sheet, 0, node));
                                pending.sources.borrow_mut().links.insert(node, sheet);
                            }
                        }
                        PendingResource::Import(import) => {
                            if let Some(sheet) = fetched_sheet(&loaded, &fetched.url) {
                                imports.extend(Import::all_in(&sheet, import.depth, import.owner));
                                pending
                                    .sources
                                    .borrow_mut()
                                    .imports
                                    .insert(import.url, sheet);
                            }
                        }
                        PendingResource::Script(node) => {
                            let source = decode_text(&loaded.bytes, loaded.charset.as_deref());
                            pending.scripts.insert(node, source);
                            resumed = true;
                        }
                        PendingResource::Module(url) => {
                            modules.push((url, super::modules::module_response(Ok(&loaded))));
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
                                pictures.push((node, src.clone(), Some(image.natural_size())));
                                pending
                                    .images
                                    .insert(node, image.clone().for_element(density));
                                pending.picture_sources.insert(node, (src, density));
                            }
                            // Said once already, where it was decoded.
                            None => {
                                pictures.push((node, src, None));
                            }
                        },
                    }
                }
            }
            Err(error) => {
                tracing::warn!(url = %fetched.url, %error, "subresource failed to load");
                for resource in wanted {
                    match resource {
                        PendingResource::Image(node, src, _) => pictures.push((node, src, None)),
                        PendingResource::Module(url) => modules.push((url, Err(error.clone()))),
                        _ => {}
                    }
                }
            }
        }
        self.record_pictures(tab, pictures);

        self.request_imports(tab, imports);
        // A module that completes a graph runs here, and what it imports is
        // asked for before the load decides whether it is over.
        if !modules.is_empty() {
            self.deliver_modules(tab, modules);
            self.request_found(tab);
        }
        if resumed {
            self.resume_parse(tab);
        }

        if self
            .pending(tab)
            .is_some_and(|pending| pending.outstanding.is_empty())
        {
            self.finish_load(tab);
        }
    }

    /// Carry on parsing a document that was stopped at a script, now that the
    /// script has arrived.
    ///
    /// The tree is the tab's and the parser borrows it back, so the page the
    /// reader is looking at grows rather than being replaced. It may stop again
    /// at the next script — a page with five bundles in its head stops five
    /// times — and each stop is another piece of the document on screen.
    fn resume_parse(&mut self, tab: TabId) {
        loop {
            let Some(pending) = self.pending_mut(tab) else {
                return;
            };
            let Some(parser) = pending.parse.as_mut() else {
                return;
            };
            let Some(blocked) = parser.blocked_on() else {
                return;
            };
            // A style sheet still loading blocks scripts (HTML §4.2.4 "has a
            // style sheet that is blocking scripts"): a script runs against
            // the style the sheets before it give, which it reads through
            // `getComputedStyle`. The sheet that settles resumes the parse.
            let sheets_loading = pending.outstanding.values().flatten().any(|resource| {
                matches!(
                    resource,
                    PendingResource::Stylesheet(_) | PendingResource::Import(_)
                )
            });
            if sheets_loading {
                return;
            }
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
                self.step_parse(tab, |document| parser.skip_script(document));
                self.after_parse_step(tab, parser);
                continue;
            };

            let mut parser = pending.parse.take().expect("just borrowed");
            self.step_parse(tab, |document| parser.supply_script(document, &source));
            self.after_parse_step(tab, parser);
        }
    }

    /// Run one step of a stopped parse over the tab's own tree.
    fn step_parse(&mut self, tab: TabId, run: impl FnOnce(&mut otlyra_dom::Document)) {
        if let Some(page) = self.page_mut(tab) {
            page.with_document(run);
            // The tree is longer than it was, so everything style and layout
            // made of it is out of date.
            page.document_changed();
        }
    }

    /// Put the parser back, or finish the parse if it has nothing left to wait
    /// for.
    fn after_parse_step(&mut self, tab: TabId, parser: otlyra_html::HtmlParser) {
        // What the parse just added may link sheets and show pictures that the
        // part before it did not; those are asked for now, while the load is
        // still waiting, or they would never be asked for at all.
        self.request_found(tab);
        if parser.blocked_on().is_some() {
            if let Some(pending) = self.pending_mut(tab) {
                pending.parse = Some(parser);
            }
            // Module scripts the parse just passed start their fetches now.
            self.request_modules(tab);
            let Some(parser) = self
                .pending_mut(tab)
                .and_then(|pending| pending.parse.take())
            else {
                return;
            };
            // Stopped again, at the next script. Ask for that one.
            let src = self.page(tab).and_then(|page| {
                let node = parser.blocked_on()?;
                let src = page.document().attr(node, "src")?.trim().to_owned();
                (!src.is_empty()).then_some((node, src))
            });
            if let Some((node, src)) = src {
                self.request_for_load(
                    tab,
                    std::iter::once((src, PendingResource::Script(node), ResourceKind::Script)),
                );
            }
            if let Some(pending) = self.pending_mut(tab) {
                pending.parse = Some(parser);
            }
            return;
        }

        // Nothing left to wait for: the last byte is parsed and the load events
        // are due.
        let mut runner = None;
        if let Some(page) = self.page_mut(tab) {
            page.with_document(|document| runner = parser.finish(document));
            page.document_changed();
        }
        if let Some(pending) = self.pending_mut(tab)
            && let Some(runner) = runner
        {
            pending.runner = Some(runner);
        }
        self.request_found(tab);
        self.request_modules(tab);
    }

    /// Ask for the sheets and pictures the tab's document names that have not
    /// been asked for yet: the ones a parse that stopped at a script had not
    /// reached when the document was first put on screen.
    pub(super) fn request_found(&mut self, tab: TabId) {
        let Some(page) = self.page(tab) else {
            return;
        };
        let Some(pending) = self.pending(tab) else {
            return;
        };
        let (here, base) = (page.url().to_string(), page.base_url().clone());
        let document = page.document();

        let links: Vec<_> = otlyra_css::cascade::stylesheet_links(document)
            .into_iter()
            .filter(|link| !pending.links_asked.contains(&link.node))
            .take(STYLESHEET_LIMIT.saturating_sub(pending.sheets_asked))
            .collect();
        let pictures: Vec<_> = otlyra_layout::image_sources(document, self.environment.viewport)
            .into_iter()
            .filter(|source| !pending.pictures_asked.contains(&source.node))
            .take(IMAGE_LIMIT.saturating_sub(pending.pictures_asked.len()))
            .collect();
        let imports: Vec<Import> = otlyra_css::cascade::style_element_imports(document, &base)
            .into_iter()
            .filter(|(_, url)| !pending.imports_asked.contains(url))
            .map(|(owner, url)| Import {
                url,
                depth: 1,
                owner,
            })
            .collect();
        if links.is_empty() && pictures.is_empty() && imports.is_empty() {
            return;
        }

        let mut wanted = Vec::new();
        let mut ready = Images::default();
        for source in &pictures {
            // Decoded for an earlier page and still here: no request at all.
            let cached = Self::subresource_url(&here, &base, &source.src)
                .and_then(|url| self.images.get(&url));
            match cached {
                Some(data) => {
                    ready.insert(source.node, data.for_element(source.density));
                }
                None => wanted.push((
                    source.src.clone(),
                    PendingResource::Image(source.node, source.src.clone(), source.density),
                    ResourceKind::Image,
                )),
            }
        }
        for link in &links {
            wanted.push((
                link.href.clone(),
                PendingResource::Stylesheet(link.node),
                ResourceKind::Stylesheet,
            ));
        }
        tracing::debug!(
            sheets = links.len(),
            pictures = pictures.len(),
            imports = imports.len(),
            "found more to ask for as the parse went on"
        );
        if let Some(pending) = self.pending_mut(tab) {
            pending.sheets_asked += links.len();
            pending
                .links_asked
                .extend(links.iter().map(|link| link.node));
            pending
                .pictures_asked
                .extend(pictures.iter().map(|source| source.node));
            pending.images.extend(ready);
        }
        self.request_for_load(tab, wanted.into_iter());
        self.request_imports(tab, imports);
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
    pub(in crate::browser) fn blocked_on_style(&self, tab: TabId) -> bool {
        let Some(pending) = self.pending(tab) else {
            return false;
        };
        let document = self.page(tab).map(PageScene::document);
        let viewport = self.environment.viewport;
        pending
            .outstanding
            .values()
            .flatten()
            .any(|resource| match resource {
                // A sheet written for another medium blocks nothing: a
                // print-only one holds the screen for a page it will never
                // style. What it says it is for is asked of the same matcher
                // the cascade uses, so the two cannot come to disagree. An
                // import blocks what the sheet it began in blocks.
                PendingResource::Stylesheet(node)
                | PendingResource::Import(Import { owner: node, .. }) => {
                    document.is_none_or(|document| {
                        crate::media_of_sheet_owner(document, *node).is_none_or(|media| {
                            otlyra_css::cascade::media_condition_matches(&media, viewport)
                        })
                    })
                }
                PendingResource::Image(..)
                | PendingResource::Script(..)
                | PendingResource::ScriptSource(..)
                | PendingResource::Module(..) => false,
            })
    }

    /// Everything the page asked for has arrived or failed: build it for real.
    fn finish_load(&mut self, tab: TabId) {
        // A new page names its own backgrounds; what the last one asked for is not
        // an answer for this one.
        self.background_requests.clear();

        let interface = self.environment.interface;
        let viewport = self.environment.viewport;
        self.generations += 1;
        let generation = self.generations;
        // What the page's script asked for while it loaded — an
        // `XMLHttpRequest`, a script it inserted, an `import()` — is still
        // the same document's: its answers are not a stale page's.
        if let Some(slot) = self.slots.get(&tab)
            && slot.pending.is_some()
        {
            let from = slot.generation;
            for fetch in self.page_fetches.values_mut() {
                fetch.rebase(tab, from, generation);
            }
            for fetch in self.module_fetches.values_mut() {
                fetch.rebase(tab, from, generation);
            }
        }
        let Some(slot) = self.slots.get_mut(&tab) else {
            return;
        };
        let Some(mut pending) = slot.pending.take() else {
            return;
        };
        let scroll = pending.restore_scroll;
        let finishing = std::time::Instant::now();

        // The scripts the page linked to, in the order it named them, then the
        // load events that were waiting for them. This happens before the scene
        // is rebuilt, so whatever they did to the document is in the first frame
        // the reader sees rather than in a second one after a flash.
        let (mut document, url) = slot.page.take().map(PageScene::into_document).unzip();
        let mut scripts_ran = std::time::Duration::ZERO;
        let pictures = std::mem::take(&mut slot.picture_events);
        if let (Some(document), Some(runner)) = (document.as_mut(), pending.runner.as_mut()) {
            for node in &pending.script_order {
                if let Some(source) = pending.scripts.get(node) {
                    runner.run_external(source, *node, document);
                }
            }
            scripts_ran = finishing.elapsed();
            // The pictures that arrived or failed during the load, before the
            // window's load, which waits for them.
            for (node, loaded) in pictures {
                runner.dispatch_input(document, node, if loaded { "load" } else { "error" }, "{}");
            }
            runner.release_load_event(document);
        }
        let load_event = finishing.elapsed();
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
        let settled = finishing.elapsed();
        // The isolate stays with the tab: its timers are still coming due and
        // its listeners are still waiting.
        slot.scripts = pending.runner.take();

        let url_shown = url.clone();
        if let (Some(document), Some(url)) = (document, url) {
            let mut page = PageScene::with_resources(
                document,
                url,
                // The script styler keeps its share, so the page takes a
                // copy only when one is still held.
                std::rc::Rc::try_unwrap(pending.sources)
                    .map(std::cell::RefCell::into_inner)
                    .unwrap_or_else(|shared| shared.borrow().clone()),
                pending.images,
                pending.picture_sources,
            );
            if let Some(styler) = pending.script_styler {
                page.set_script_styler(styler);
            }
            slot.page = Some(shown(page, interface));
        }
        let since = |at: Option<std::time::Instant>| {
            at.map_or(0, |at| at.duration_since(pending.started).as_millis())
        };
        tracing::info!(
            target: "page.timing",
            url = url_shown.as_ref().map_or("", url::Url::as_str),
            arrived_ms = since(pending.timing.arrived),
            build_ms = pending.timing.build.as_millis(),
            built_ms = since(pending.timing.built),
            everything_ms = since(Some(finishing)),
            scripts_ms = scripts_ran.as_millis(),
            load_event_ms = (load_event - scripts_ran).as_millis(),
            settle_ms = (settled - load_event).as_millis(),
            scene_ms = (finishing.elapsed() - settled).as_millis(),
            total_ms = pending.started.elapsed().as_millis(),
            "a load finished"
        );
        slot.generation = generation;
        slot.swept_at_build = None;
        // What the load asked for and did not get stays asked: a picture that
        // failed is not fetched again by the sweep that follows each frame.
        slot.pictures_asked = slot
            .page
            .as_ref()
            .map(|page| {
                otlyra_layout::image_sources(page.document(), viewport)
                    .into_iter()
                    .filter(|source| pending.pictures_asked.contains(&source.node))
                    .map(|source| (source.node, source.src))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(page) = slot.page.as_mut() {
            page.set_scroll(scroll);
            // A focus the load's scripts asked for — a sign-in page focusing
            // its first field — is the page's once it is built.
            if let Some(scripts) = slot.scripts.as_mut() {
                super::after_script_turn(page, scripts.as_mut());
            }
        }

        self.follow_script_navigation(tab);
    }

    /// Show where `history.pushState` and its kin moved `tab`'s document.
    ///
    /// The glue only moves a document within its own origin, so this is not
    /// a navigation and earns no hop; the page keeps its document and its
    /// scripts, and only its address changes.
    fn follow_history_move(&mut self, tab: TabId) {
        let changes = self
            .slots
            .get_mut(&tab)
            .and_then(|slot| slot.scripts.as_mut())
            .map(|scripts| scripts.take_url_changes())
            .unwrap_or_default();
        for change in changes {
            let Ok(parsed) = url::Url::parse(&change.url) else {
                continue;
            };
            let Some(page) = self.page_mut(tab) else {
                return;
            };
            if page.url().origin() != parsed.origin() {
                tracing::warn!(url = %change.url, "a history entry left its document's origin");
                continue;
            }
            page.set_url(parsed);
            self.fetcher.set_initiator(tab.0, Some(change.url.clone()));
            self.events.push(WorldEvent::Moved {
                tab,
                url: change.url,
                how: change.kind,
            });
        }
    }

    /// Move `tab`'s document `delta` entries through the history entries it
    /// made, for the back and forward buttons. False when it cannot, and
    /// the entry is to be loaded instead.
    pub(in crate::browser) fn traverse_in_page(&mut self, tab: TabId, delta: i32) -> bool {
        self.with_page_runner(tab, |runner, document| {
            runner.traverse_history(document, delta)
        })
        .unwrap_or(false)
    }

    /// Go where the page's script said to go.
    ///
    /// A redirector page is a page whose whole content is an instruction to be
    /// somewhere else — a sign-in bounce, a shortener, an old address. Script
    /// asks; the browser decides, and decides here rather than inside the
    /// isolate, because the isolate is holding the document a navigation
    /// destroys. What is decided here is whether the hop is allowed and where it
    /// goes; the going is the window's, which keeps the history.
    pub(super) fn follow_script_navigation(&mut self, tab: TabId) {
        self.follow_history_move(tab);
        // This tab's, not the thread's. A navigation asked for by one tab was
        // once answerable by whichever tab was being pumped when it was noticed.
        let Some(request) = self
            .slots
            .get_mut(&tab)
            .and_then(|slot| slot.scripts.as_mut())
            .and_then(|scripts| scripts.take_navigation())
        else {
            return;
        };
        if self.environment.active != Some(tab) {
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

        let to = match request {
            otlyra_script::dom::Navigation::Reload => ScriptNavigation::Reload,
            otlyra_script::dom::Navigation::Url { href, replace } => {
                // Against the document's base URL: what `location` parses a
                // URL relative to is the settings object's API base URL, which
                // for a document is its base URL.
                let page = self.page(tab);
                let here = page.map(|page| page.url().to_string());
                let url = page.and_then(|page| page.resolve(&href)).unwrap_or(href);
                if here.as_deref() == Some(url.as_str()) {
                    // `location.href = location.href` is a reload, and a page
                    // that does it on every load is the loop above.
                    tracing::debug!(%url, "script navigated to where it already is");
                }
                ScriptNavigation::Url { url, replace }
            }
            otlyra_script::dom::Navigation::Submit { form } => {
                let staged = self
                    .page_mut(tab)
                    .is_some_and(|page| page.submit_from_script(form));
                if !staged {
                    return;
                }
                ScriptNavigation::Submit
            }
        };
        self.events.push(WorldEvent::Navigate { tab, to });
    }
    /// The address a subresource is actually fetched from, or `None` if `href`
    /// is not a URL or the page may not reach it.
    ///
    /// `href` is resolved against `base`, and whether it may be reached is asked
    /// of the document at `document` — the two differ once a base element points
    /// the page's addresses somewhere else, and a base cannot lend the page an
    /// authority it does not have. A document fetched over the network may not
    /// reach a `file:` URL, the same rule that governs where it may navigate: a
    /// subresource is a request the page chose to make, and a page from the
    /// internet reading the disk is the failure that rule exists to prevent.
    pub(in crate::browser) fn subresource_url(
        document: &str,
        base: &Url,
        href: &str,
    ) -> Option<String> {
        let url = base.join(href).ok()?;
        Self::may_reach(document, &url).then(|| url.into())
    }

    /// Whether the document at `document` may fetch `url`, an address already
    /// absolute: a sheet's `url()`s, each resolved as the sheet was parsed.
    ///
    /// The rule is the one [`Self::subresource_url`] describes.
    pub(in crate::browser) fn may_reach(document: &str, url: &Url) -> bool {
        let allowed = otlyra_net::may_navigate(Some(document), url);
        if !allowed {
            tracing::warn!(%url, %document, "subresource refused by scheme policy");
        }
        allowed
    }
}

/// The address a document was read from, as a URL.
///
/// The fetcher hands back the address it finally read, which is one; a document
/// whose address somehow is not is treated as one from nowhere.
fn document_url(final_url: &str) -> Url {
    Url::parse(final_url).unwrap_or_else(|error| {
        tracing::warn!(%final_url, %error, "a document's address does not parse");
        otlyra_css::cascade::about_blank()
    })
}

/// A stylesheet response as the cascade takes it: decoded, and at the address it
/// finally came from, which its relative addresses resolve against.
///
/// Where a loader names no final address, the one asked for stands in.
fn fetched_sheet(loaded: &crate::fetcher::Loaded, requested: &str) -> Option<FetchedSheet> {
    let url = Url::parse(&loaded.final_url)
        .or_else(|_| Url::parse(requested))
        .inspect_err(
            |error| tracing::warn!(%requested, %error, "a stylesheet's address does not parse"),
        )
        .ok()?;
    Some(FetchedSheet {
        url,
        text: decode_text(&loaded.bytes, loaded.charset.as_deref()),
    })
}

/// A page scene the way this browser shows it.
///
/// With the interface hidden the picture is going to be put beside another
/// browser's, taken with its scrollbars hidden, so ours are hidden too — on every
/// scene a load builds, not only the first: the one the reader ends up with is the
/// scene rebuilt once everything arrived, and it used to draw them again.
fn shown(mut page: PageScene, interface: bool) -> PageScene {
    if !interface {
        page.hide_scrollbars();
    }
    page
}
