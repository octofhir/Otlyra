//! The cascade: stylesheets in, one computed style per element out.
//!
//! What this owns is the *arrangement* — which sheets exist, in which origin, what
//! the device is, and the order elements are visited in. The resolution itself is
//! the style engine's, driven one element at a time from the root down, because a
//! child's style is a function of its parent's and nothing else may run in
//! between.
//!
//! Sequential on purpose. Parallel restyle is a real speed-up on a large document
//! and it is also a way to have two documents restyle at once through one global
//! pool; the plan defers it, and this is where that decision lives.

use std::collections::HashMap;

use otlyra_dom::{Document, NodeData, NodeId};
use servo_arc::Arc;
use style::context::{QuirksMode, SharedStyleContext, StyleContext, ThreadLocalStyleContext};
use style::device::Device;
use style::media_queries::{MediaList, MediaType};
use style::properties::ComputedValues;
use style::selector_parser::SnapshotMap;
use style::shared_lock::{SharedRwLock, StylesheetGuards};
use style::stylesheets::{AllowImportRules, DocumentStyleSheet, Origin, Stylesheet, UrlExtraData};
use style::stylist::Stylist;
use style::traversal_flags::TraversalFlags;

use crate::stylo_dom::{NodeRef, StyleData, Tree, TreeScope};

/// Our user-agent stylesheet, in the language it belongs to.
pub const UA_STYLESHEET: &str = include_str!("ua.css");

/// A computed style per element, and the sheets that produced them.
pub struct StyledDocument {
    /// The engine's per-element state, which owns the computed values.
    pub style_data: StyleData,
    /// The computed style of each element, by node.
    styles: HashMap<NodeId, Arc<ComputedValues>>,
}

impl StyledDocument {
    /// The computed style of one element, if it has one.
    pub fn style_of(&self, node: NodeId) -> Option<&Arc<ComputedValues>> {
        self.styles.get(&node)
    }

    /// How many elements got a style.
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    /// Whether nothing was styled.
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }
}

/// The viewport a document is styled against.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Viewport {
    /// Width in CSS pixels.
    pub width: f32,
    /// Height in CSS pixels.
    pub height: f32,
    /// Device pixels per CSS pixel.
    pub scale: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            scale: 1.0,
        }
    }
}

/// Style every element in `document` against `viewport`.
///
/// Author sheets come from the document's own `<style>` elements; the user-agent
/// sheet is ours. Both go into the same stylist, in their own origins, and the
/// cascade decides between them — which is the whole point of using a real cascade
/// rather than a table of defaults that author rules have to be merged into by
/// hand.
pub fn style_document(document: &Document, viewport: Viewport) -> StyledDocument {
    style_document_with(document, viewport, &ExternalSheets::default())
}

/// Style every element in `document`, with the stylesheets its `<link>` elements
/// asked for already fetched.
///
/// The fetch is the caller's: styling is synchronous and must not wait on a
/// network, so what arrives here is text that has already been got. A link with
/// nothing fetched for it simply contributes nothing, which is what a browser does
/// with a stylesheet that failed to load.
pub fn style_document_with(
    document: &Document,
    viewport: Viewport,
    external: &ExternalSheets,
) -> StyledDocument {
    Styler::new(document, viewport, external).style(document)
}

/// The parsed stylesheets and the machinery that cascades them, kept between
/// restyles.
///
/// Parsing a page's CSS again on every resize is the cost this exists to remove:
/// the sheets have not changed, and neither has which rule beats which. What a new
/// viewport can change is which media queries match and what `vw` resolves to, and
/// the engine can answer whether either actually did — so most resizes turn out to
/// need no cascade at all.
pub struct Styler {
    lock: SharedRwLock,
    stylist: Stylist,
    quirks_mode: QuirksMode,
    viewport: Viewport,
}

impl std::fmt::Debug for Styler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Styler")
            .field("viewport", &self.viewport)
            .finish_non_exhaustive()
    }
}

impl Styler {
    /// Parse the user-agent sheet and the document's own, once.
    pub fn new(document: &Document, viewport: Viewport, external: &ExternalSheets) -> Self {
        let _span = tracing::info_span!("parse_stylesheets").entered();
        enable_features();

        let lock = SharedRwLock::new();
        let url = base_url();
        let quirks_mode = match document.quirks_mode() {
            html5ever::interface::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
            html5ever::interface::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
            html5ever::interface::QuirksMode::Quirks => QuirksMode::Quirks,
        };

        let mut stylist = Stylist::new(device_for(viewport, quirks_mode), quirks_mode);
        stylist.append_stylesheet(
            DocumentStyleSheet(Arc::new(parse_sheet(
                UA_STYLESHEET,
                Origin::UserAgent,
                &lock,
                &url,
                quirks_mode,
            ))),
            &lock.read(),
        );

        for source in author_stylesheets(document, external) {
            stylist.append_stylesheet(
                DocumentStyleSheet(Arc::new(parse_sheet(
                    &source,
                    Origin::Author,
                    &lock,
                    &url,
                    quirks_mode,
                ))),
                &lock.read(),
            );
        }

        Self {
            lock,
            stylist,
            quirks_mode,
            viewport,
        }
    }

    /// The viewport the last cascade ran against.
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Point the sheets at a new viewport, and say whether that changes anything.
    ///
    /// `false` means the same rules apply to the same elements with the same
    /// values, so the styles already computed still hold and the caller can go
    /// straight to layout. That is the common case: a window resized on a page
    /// with no media queries and no viewport units restyles nothing.
    ///
    /// Before the first cascade the answer is always `true`, because there is
    /// nothing yet for a resize to preserve.
    pub fn resize(&mut self, viewport: Viewport) -> bool {
        if self.viewport == viewport {
            return false;
        }

        // Asked before the device is replaced: the flag is set while cascading,
        // on the device that did the cascading.
        let used_viewport_units = self.stylist.device().used_viewport_size();
        self.viewport = viewport;

        let device = device_for(viewport, self.quirks_mode);
        let guard = self.lock.read();
        let changed = self
            .stylist
            .set_device(device, &StylesheetGuards::same(&guard));
        drop(guard);

        if !changed.is_empty() {
            // A media query that evaluates differently changes which rules are in
            // the cascade, which is a rebuild of that origin's data rather than a
            // fact about any one element.
            self.stylist.force_stylesheet_origins_dirty(changed);
            return true;
        }
        used_viewport_units
    }

    /// Compute a style for every element.
    pub fn style(&mut self, document: &Document) -> StyledDocument {
        let _span = tracing::info_span!("recalc_style").entered();

        let mut style_data = StyleData::with_lock(self.lock.clone());
        style_data.prepare(document);

        let guard = self.lock.read();
        self.stylist.flush(&StylesheetGuards::same(&guard));

        let snapshots = SnapshotMap::new();
        let shared = SharedStyleContext {
            stylist: &self.stylist,
            visited_styles_enabled: false,
            options: Default::default(),
            guards: StylesheetGuards::same(&guard),
            current_time_for_animations: 0.0,
            traversal_flags: TraversalFlags::empty(),
            snapshot_map: &snapshots,
            animations: Default::default(),
            registered_speculative_painters: &NoPainters,
        };

        let tree = Tree::styled(document, &style_data);
        let _scope = TreeScope::enter(&tree);
        let mut styles = HashMap::new();
        {
            // The engine's assertions check that a restyle happens on a thread that
            // has declared itself the layout thread — including in the destructors
            // of the context, which is why this is a guard and not two bare calls.
            // The assertion is not a formality: element data is behind interior
            // mutability that only one thread at a time may touch.
            let _layout = LayoutThread::enter();

            let mut thread_local = ThreadLocalStyleContext::new();
            let mut context = StyleContext {
                shared: &shared,
                thread_local: &mut thread_local,
            };

            let root = document.root();
            for child in document.children(root).collect::<Vec<_>>() {
                resolve(&tree, child, None, 0, &mut context, &mut styles);
            }
        }

        tracing::debug!(elements = styles.len(), "styled");
        StyledDocument { style_data, styles }
    }
}

/// Turn on the parts of the engine that ship switched off.
///
/// The style engine carries preferences from the browser it was taken from, and
/// some of them gate whether a value parses at all: with `layout.grid.enabled`
/// false, `display: grid` is not a display value and every grid on the web lays out
/// as a block. Set before the first stylesheet is parsed, because a value that did
/// not parse is not stored anywhere to be reconsidered.
fn enable_features() {
    use std::sync::Once;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        stylo_static_prefs::set_pref!("layout.grid.enabled", true);
    });
}

/// The device a viewport describes: what a media query is evaluated against and
/// what `vw` and `vh` resolve to.
fn device_for(viewport: Viewport, quirks_mode: QuirksMode) -> Device {
    Device::new(
        MediaType::screen(),
        quirks_mode,
        euclid::Size2D::new(viewport.width, viewport.height),
        euclid::Size2D::new(
            viewport.width * viewport.scale,
            viewport.height * viewport.scale,
        ),
        euclid::Scale::new(viewport.scale),
        Box::new(NoFontMetrics),
        // The initial values every cascade starts from, with the default font taken
        // from those same initial values: there is no platform font preference to
        // consult yet.
        ComputedValues::initial_values_with_font_override(
            style::properties::style_structs::Font::initial_values(),
        ),
        style::queries::values::PrefersColorScheme::Light,
        style::servo::media_features::PointerCapabilities::FINE,
        style::servo::media_features::PointerCapabilities::FINE,
    )
}

/// Declares this thread the layout thread for as long as it is held.
struct LayoutThread;

impl LayoutThread {
    fn enter() -> Self {
        style::thread_state::enter(style::thread_state::ThreadState::LAYOUT);
        Self
    }
}

impl Drop for LayoutThread {
    fn drop(&mut self) {
        style::thread_state::exit(style::thread_state::ThreadState::LAYOUT);
    }
}

/// Resolve `node`'s style, then its children's, depth first.
///
/// Depth first and parent first, because inheritance means a child cannot be
/// resolved before its parent — which is also why this is one function and not a
/// worklist.
fn resolve<'a>(
    tree: &'a Tree<'a>,
    node: NodeId,
    parent: Option<&Arc<ComputedValues>>,
    depth: usize,
    context: &mut StyleContext<'_, NodeRef<'a>>,
    styles: &mut HashMap<NodeId, Arc<ComputedValues>>,
) {
    let document = tree.document;
    let is_element = document
        .get(node)
        .is_some_and(|node| matches!(node.data, NodeData::Element(_)));

    let own_style = if is_element {
        let element = tree.node(node);
        // The ancestor filter must hold this element's ancestors — and only those —
        // when it is matched, or every selector with a combinator is fast-rejected
        // and quietly does not apply. It is a cache that changes the answer when it
        // is wrong, so the traversal keeps it in step with the walk.
        context
            .thread_local
            .bloom_filter
            .insert_parents_recovering(element, depth);
        let resolved = style::style_resolver::StyleResolverForElement::new(
            element,
            context,
            style::stylist::RuleInclusion::All,
            style::style_resolver::PseudoElementResolution::IfApplicable,
        )
        .resolve_primary_style(parent.map(|style| &**style), parent.map(|style| &**style));

        let style = resolved.style.0;
        styles.insert(node, style.clone());
        Some(style)
    } else {
        None
    };

    let inherited = own_style.as_ref().or(parent);
    let child_depth = if is_element { depth + 1 } else { depth };
    for child in document.children(node).collect::<Vec<_>>() {
        resolve(tree, child, inherited, child_depth, context, styles);
    }
}

/// Every author stylesheet in the document, in tree order.
///
/// Tree order is not decoration: two rules of equal specificity are decided by
/// which sheet came last, so a `<style>` after a `<link>` has to be appended after
/// it — which means both kinds are collected by one walk rather than one list
/// after another.
fn author_stylesheets(document: &Document, external: &ExternalSheets) -> Vec<String> {
    let mut sheets = Vec::new();
    let mut stack = vec![document.root()];

    while let Some(id) = stack.pop() {
        if let Some(element) = document.get(id).and_then(|node| node.element()) {
            match element.name.local.as_ref() {
                "style" => {
                    let mut source = String::new();
                    for child in document.children(id) {
                        if let Some(NodeData::Text(text)) =
                            document.get(child).map(|node| &node.data)
                        {
                            source.push_str(text);
                        }
                    }
                    if !source.trim().is_empty() {
                        sheets.push(source);
                    }
                }
                "link" => {
                    if let Some(source) = external.get(&id) {
                        // A `media` attribute applies to the whole sheet, and
                        // wrapping it is exactly what that means — the queries
                        // inside are then evaluated against the same device as
                        // every other one.
                        match attribute(document, id, "media").filter(|q| !q.trim().is_empty()) {
                            Some(query) => sheets.push(format!("@media {query} {{\n{source}\n}}")),
                            None => sheets.push(source.clone()),
                        }
                    }
                }
                _ => {}
            }
        }
        stack.extend(document.children(id).collect::<Vec<_>>().into_iter().rev());
    }

    sheets
}

/// The stylesheets fetched for a document, by the `<link>` element that asked for
/// each one.
pub type ExternalSheets = HashMap<NodeId, String>;

/// A stylesheet a document asks for but does not contain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StylesheetLink {
    /// The `<link>` element, which is how the fetched text finds its way back to
    /// the place in the document that decides where it cascades.
    pub node: NodeId,
    /// The address, exactly as the attribute spells it — resolving it against the
    /// document's own needs the document's address, which this crate does not know.
    pub href: String,
}

/// Every `<link rel=stylesheet>` in the document, in tree order.
///
/// `rel` is a space-separated list of keywords, and only the ones that make the
/// link a stylesheet count. `alternate` is skipped: an alternate sheet is one the
/// reader chooses, and applying it alongside the main one would style the page
/// twice over.
pub fn stylesheet_links(document: &Document) -> Vec<StylesheetLink> {
    let mut links = Vec::new();
    let mut stack = vec![document.root()];

    while let Some(id) = stack.pop() {
        if let Some(element) = document.get(id).and_then(|node| node.element())
            && element.name.local.as_ref() == "link"
        {
            let rel = attribute(document, id, "rel").unwrap_or_default();
            let mut keywords = rel.split_ascii_whitespace().map(str::to_ascii_lowercase);
            let is_sheet = keywords.clone().any(|word| word == "stylesheet");
            let alternate = keywords.any(|word| word == "alternate");

            if is_sheet
                && !alternate
                && let Some(href) = attribute(document, id, "href")
                && !href.trim().is_empty()
            {
                links.push(StylesheetLink { node: id, href });
            }
        }
        stack.extend(document.children(id).collect::<Vec<_>>().into_iter().rev());
    }

    links
}

/// One attribute of an element, by local name.
fn attribute(document: &Document, node: NodeId, name: &str) -> Option<String> {
    document
        .get(node)
        .and_then(|node| node.element())?
        .attrs
        .iter()
        .find(|attribute| attribute.name.local.as_ref() == name)
        .map(|attribute| attribute.value.to_string())
}

/// Parse one stylesheet.
fn parse_sheet(
    source: &str,
    origin: Origin,
    lock: &SharedRwLock,
    url: &UrlExtraData,
    quirks_mode: QuirksMode,
) -> Stylesheet {
    Stylesheet::from_str(
        source,
        url.clone(),
        origin,
        Arc::new(lock.wrap(MediaList::empty())),
        lock.clone(),
        None,
        None,
        quirks_mode,
        AllowImportRules::No,
    )
}

/// The base every sheet is parsed against.
///
/// Relative `url()` in a stylesheet resolves against the document it came from,
/// and we do not thread that through yet — so this is a base that cannot resolve
/// to anything rather than one that resolves to the wrong thing.
pub(crate) fn base_url() -> UrlExtraData {
    UrlExtraData(Arc::new(
        url::Url::parse("about:blank").expect("about:blank parses"),
    ))
}

/// No paint worklets, which is a web feature nothing here implements.
struct NoPainters;

impl style::context::RegisteredSpeculativePainters for NoPainters {
    fn get(
        &self,
        _name: &style::Atom,
    ) -> Option<&dyn style::context::RegisteredSpeculativePainter> {
        None
    }
}

/// Font metrics for the queries that need them — `ex`, `ch`, `ic` units and the
/// `font-size` keywords' relationship to the actual face.
///
/// Reporting none makes the engine fall back to ratios of the font size, which is
/// what it does when a platform cannot answer. Real metrics live in the text
/// crate, and threading them here is worth doing once anything depends on them.
#[derive(Debug)]
struct NoFontMetrics;

impl style::device::servo::FontMetricsProvider for NoFontMetrics {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        _font: &style::properties::style_structs::Font,
        _base_size: style::values::computed::CSSPixelLength,
        _flags: style::values::specified::font::QueryFontMetricsFlags,
    ) -> style::font_metrics::FontMetrics {
        Default::default()
    }

    fn base_size_for_generic(
        &self,
        _generic: style::values::computed::font::GenericFontFamily,
    ) -> style::values::computed::Length {
        style::values::computed::Length::new(16.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Style a document and hand back the computed values of the first element
    /// matching `selector`.
    fn computed(html: &str, selector: &str) -> Arc<ComputedValues> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styled = style_document(&document, Viewport::default());
        let node = crate::stylo_dom::select(&document, selector)
            .expect("the selector should parse")
            .into_iter()
            .next()
            .expect("something should match");
        styled.style_of(node).expect("a styled element").clone()
    }

    fn colour(style: &ComputedValues) -> (u8, u8, u8) {
        let colour = style
            .clone_color()
            .to_color_space(style::color::ColorSpace::Srgb);
        (
            (colour.components.0 * 255.0).round() as u8,
            (colour.components.1 * 255.0).round() as u8,
            (colour.components.2 * 255.0).round() as u8,
        )
    }

    /// Style a document with one fetched sheet per `<link>`, in the order the
    /// links appear, and read one element's computed style back.
    fn computed_with_links(html: &str, sources: &[&str], selector: &str) -> Arc<ComputedValues> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let links = stylesheet_links(&document);
        assert_eq!(links.len(), sources.len(), "one source per link");
        let external: ExternalSheets = links
            .iter()
            .zip(sources)
            .map(|(link, source)| (link.node, (*source).to_owned()))
            .collect();

        let styled = style_document_with(&document, Viewport::default(), &external);
        let node = crate::stylo_dom::select(&document, selector)
            .expect("the selector should parse")
            .into_iter()
            .next()
            .expect("something should match");
        styled.style_of(node).expect("a styled element").clone()
    }

    /// A resize that no rule reads changes nothing, and the caller is told so:
    /// this is what turns a window drag into a relayout rather than a re-cascade of
    /// the whole document.
    #[test]
    fn a_resize_only_restyles_when_a_rule_reads_the_viewport() {
        let plain =
            otlyra_html::parse(b"<style>p { color: red }</style><p>x", Some("utf-8")).document;
        let mut styler = Styler::new(&plain, Viewport::default(), &ExternalSheets::new());
        styler.style(&plain);
        assert!(
            !styler.resize(Viewport {
                width: 500.0,
                ..Viewport::default()
            }),
            "nothing in this document reads the viewport"
        );
        assert!(!styler.resize(Viewport::default()), "nor going back");

        let queried = otlyra_html::parse(
            b"<style>@media (min-width: 800px) { p { color: red } }</style><p>x",
            Some("utf-8"),
        )
        .document;
        let mut styler = Styler::new(&queried, Viewport::default(), &ExternalSheets::new());
        styler.style(&queried);
        assert!(
            styler.resize(Viewport {
                width: 400.0,
                ..Viewport::default()
            }),
            "the query stopped matching"
        );
    }

    /// A viewport unit is read while cascading rather than while matching, so the
    /// engine only knows it was used once it has been.
    #[test]
    fn a_viewport_unit_makes_every_resize_a_restyle() {
        let document =
            otlyra_html::parse(b"<style>p { width: 50vw }</style><p>x", Some("utf-8")).document;
        let mut styler = Styler::new(&document, Viewport::default(), &ExternalSheets::new());
        styler.style(&document);

        assert!(styler.resize(Viewport {
            width: 500.0,
            ..Viewport::default()
        }));
    }

    /// A page that keeps its CSS in a file is the common case, and the sheet has
    /// to reach the cascade as an author sheet like any other.
    #[test]
    fn a_linked_stylesheet_styles_the_document() {
        let styled = computed_with_links(
            "<link rel=stylesheet href=site.css><body><p>text",
            &["p { color: rgb(0, 128, 0) }"],
            "p",
        );
        assert_eq!(colour(&styled), (0, 128, 0));
    }

    /// Equal specificity is decided by source order, and a link is at the place in
    /// the document where it is written — not before every `<style>` or after them.
    #[test]
    fn a_link_cascades_where_it_appears_in_the_document() {
        let link_last = computed_with_links(
            "<style>p { color: rgb(255, 0, 0) }</style>\
             <link rel=stylesheet href=a.css><body><p>x",
            &["p { color: rgb(0, 0, 255) }"],
            "p",
        );
        assert_eq!(colour(&link_last), (0, 0, 255));

        let style_last = computed_with_links(
            "<link rel=stylesheet href=a.css>\
             <style>p { color: rgb(255, 0, 0) }</style><body><p>x",
            &["p { color: rgb(0, 0, 255) }"],
            "p",
        );
        assert_eq!(colour(&style_last), (255, 0, 0));
    }

    /// Which links are stylesheets at all: `rel` is a list of keywords, an
    /// alternate sheet is one the reader has to choose, and a link with no `href`
    /// asks for nothing.
    #[test]
    fn only_the_links_that_are_stylesheets_are_collected() {
        let document = otlyra_html::parse(
            b"<link rel=icon href=favicon.ico>\
              <link rel=\"STYLESHEET\" href=one.css>\
              <link rel=\"alternate stylesheet\" href=dark.css>\
              <link rel=stylesheet>\
              <link rel=\"preload stylesheet\" href=two.css>",
            Some("utf-8"),
        )
        .document;

        let hrefs: Vec<String> = stylesheet_links(&document)
            .into_iter()
            .map(|link| link.href)
            .collect();
        assert_eq!(hrefs, vec!["one.css".to_owned(), "two.css".to_owned()]);
    }

    /// A `media` attribute applies to the whole sheet, so a sheet for print does
    /// nothing on screen.
    #[test]
    fn a_media_attribute_gates_the_whole_sheet() {
        let screen = computed_with_links(
            "<link rel=stylesheet href=a.css media=screen><body><p>x",
            &["p { color: rgb(0, 128, 0) }"],
            "p",
        );
        assert_eq!(colour(&screen), (0, 128, 0));

        let print = computed_with_links(
            "<link rel=stylesheet href=a.css media=print><body><p>x",
            &["p { color: rgb(0, 128, 0) }"],
            "p",
        );
        assert_ne!(colour(&print), (0, 128, 0));
    }

    /// A sheet that failed to load contributes nothing, and the rest of the page
    /// is styled anyway.
    #[test]
    fn a_link_with_nothing_fetched_for_it_is_ignored() {
        let document = otlyra_html::parse(
            b"<link rel=stylesheet href=missing.css><body><p>x",
            Some("utf-8"),
        )
        .document;
        let styled = style_document_with(&document, Viewport::default(), &ExternalSheets::new());
        let node = crate::stylo_dom::select(&document, "p")
            .expect("the selector should parse")
            .into_iter()
            .next()
            .expect("a paragraph");
        assert!(styled.style_of(node).is_some());
    }

    #[test]
    fn the_user_agent_sheet_applies_without_any_author_css() {
        let heading = computed("<body><h1>title", "h1");
        assert_eq!(heading.clone_font_size().used_size().px(), 32.0);
        assert_eq!(
            heading.clone_display(),
            style::values::computed::Display::Block
        );
    }

    /// The point of the whole exercise: a rule in the document changes the page.
    #[test]
    fn an_author_rule_beats_the_user_agent_sheet() {
        let styled = computed(
            "<style>p { color: rgb(255, 0, 0); font-size: 20px }</style><body><p>text",
            "p",
        );
        assert_eq!(colour(&styled), (255, 0, 0));
        assert_eq!(styled.clone_font_size().used_size().px(), 20.0);
    }

    #[test]
    fn specificity_decides_between_author_rules() {
        let styled = computed(
            "<style>p { color: rgb(0,0,255) } .note { color: rgb(0,128,0) } \
             p.note { color: rgb(255,0,0) }</style><body><p class=note>x",
            "p",
        );
        assert_eq!(colour(&styled), (255, 0, 0), "p.note is the most specific");
    }

    #[test]
    fn a_later_rule_of_equal_specificity_wins() {
        let styled = computed(
            "<style>p { color: rgb(0,0,255) } p { color: rgb(0,128,0) }</style><body><p>x",
            "p",
        );
        assert_eq!(colour(&styled), (0, 128, 0));
    }

    #[test]
    fn important_beats_specificity() {
        let styled = computed(
            "<style>p { color: rgb(0,128,0) !important } p#x { color: rgb(0,0,255) }\
             </style><body><p id=x>text",
            "p",
        );
        assert_eq!(colour(&styled), (0, 128, 0));
    }

    #[test]
    fn colour_inherits_and_display_does_not() {
        let styled = computed(
            "<style>div { color: rgb(0,128,0); display: block }</style>\
             <body><div><span>text</span></div>",
            "span",
        );
        assert_eq!(colour(&styled), (0, 128, 0), "colour inherits");
        assert_eq!(
            styled.clone_display(),
            style::values::computed::Display::Inline,
            "display does not"
        );
    }

    /// `em` resolves against the parent's font size, which is the thing a table of
    /// defaults cannot do and a cascade does for free.
    #[test]
    fn relative_units_resolve_against_the_parent() {
        let styled = computed(
            "<style>div { font-size: 20px } div p { font-size: 1.5em }</style>\
             <body><div><p>text",
            "p",
        );
        assert_eq!(styled.clone_font_size().used_size().px(), 30.0);
    }

    #[test]
    fn a_style_attribute_beats_every_rule() {
        let styled = computed(
            "<style>p { color: rgb(0,0,255) !important }</style>\
             <body><p style='color: rgb(255,0,0)'>text",
            "p",
        );
        // The author's `!important` still wins over an ordinary inline
        // declaration, which is what the cascade order says.
        assert_eq!(colour(&styled), (0, 0, 255));
    }

    #[test]
    fn an_invalid_declaration_is_dropped_and_the_rest_survives() {
        let styled = computed(
            "<style>p { color: nonsense; font-size: 22px }</style><body><p>x",
            "p",
        );
        assert_eq!(styled.clone_font_size().used_size().px(), 22.0);
        assert_eq!(colour(&styled), (0, 0, 0), "the bad colour was ignored");
    }

    #[test]
    fn custom_properties_and_calc_work() {
        let styled = computed(
            "<style>:root { --size: 12px } p { font-size: calc(var(--size) * 2) }\
             </style><body><p>x",
            "p",
        );
        assert_eq!(styled.clone_font_size().used_size().px(), 24.0);
    }

    #[test]
    fn a_media_query_is_evaluated_against_the_viewport() {
        let document = otlyra_html::parse(
            b"<style>@media (min-width: 800px) { p { font-size: 30px } }</style><body><p>x",
            Some("utf-8"),
        )
        .document;

        let wide = style_document(
            &document,
            Viewport {
                width: 1000.0,
                ..Viewport::default()
            },
        );
        let narrow = style_document(
            &document,
            Viewport {
                width: 500.0,
                ..Viewport::default()
            },
        );

        let paragraph = crate::stylo_dom::select(&document, "p").expect("selector")[0];
        assert_eq!(
            wide.style_of(paragraph)
                .expect("styled")
                .clone_font_size()
                .used_size()
                .px(),
            30.0
        );
        assert_eq!(
            narrow
                .style_of(paragraph)
                .expect("styled")
                .clone_font_size()
                .used_size()
                .px(),
            16.0,
            "the rule does not apply below its breakpoint"
        );
    }
}
