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

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use otlyra_dom::{Document, NodeData, NodeId};
use servo_arc::Arc;
use style::context::{QuirksMode, SharedStyleContext, StyleContext, ThreadLocalStyleContext};
use style::device::Device;
use style::media_queries::{MediaList, MediaType};
use style::properties::ComputedValues;
use style::selector_parser::SnapshotMap;
use style::shared_lock::{Locked, SharedRwLock, StylesheetGuards};
use style::stylesheets::import_rule::{
    ImportLayer, ImportRule, ImportSheet, ImportSupportsCondition,
};
use style::stylesheets::{
    AllowImportRules, DocumentStyleSheet, Origin, Stylesheet, StylesheetLoader, UrlExtraData,
};
use style::stylist::Stylist;
use style::traversal_flags::TraversalFlags;
use style::values::CssUrl;
use url::Url;

use crate::stylo_dom::{NodeRef, StyleData, Tree, TreeScope};

/// Our user-agent stylesheet, in the language it belongs to.
pub const UA_STYLESHEET: &str = include_str!("ua.css");

/// The user-agent rules HTML adds in quirks mode, and only there (HTML §15, each
/// "In quirks mode, the following rules are also expected to apply").
pub const QUIRKS_STYLESHEET: &str = include_str!("quirks.css");

/// A computed style per element, and the sheets that produced them.
#[allow(missing_debug_implementations)]
pub struct StyledDocument {
    /// The engine's per-element state, which owns the computed values.
    pub style_data: StyleData,
    /// The computed style of each element, by node.
    styles: HashMap<NodeId, Arc<ComputedValues>>,
    /// The controls whose own look an author rule has taken away.
    devolved: std::collections::HashSet<NodeId>,
}

impl StyledDocument {
    /// The computed style of one element, if it has one.
    pub fn style_of(&self, node: NodeId) -> Option<&Arc<ComputedValues>> {
        self.styles.get(&node)
    }

    /// Whether this control is drawn by the page rather than as a widget.
    pub fn is_devolved(&self, node: NodeId) -> bool {
        self.devolved.contains(&node)
    }

    /// Every state bit an element held when its style was computed.
    pub fn state_of(&self, node: NodeId) -> stylo_dom::ElementState {
        self.style_data.state_of(node)
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

/// Which of the two palettes the environment is asking a page for.
///
/// A preference of the reader's, not a property of the document: what
/// `prefers-color-scheme` answers, and nothing more. A page that does not ask
/// looks the same either way.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
    /// Dark text on a light background.
    #[default]
    Light,
    /// Light text on a dark background.
    Dark,
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
    /// What the reader has asked the default font size to be, as a multiple.
    ///
    /// A preference rather than a property: it changes what `medium` computes
    /// to, which is what every page that does not name a size inherits — and
    /// which a page that *does* name one still overrides, exactly as it would
    /// override any other default. Scaling the used sizes afterwards instead
    /// would enlarge a `1px` hairline along with the prose.
    pub text_scale: f32,
    /// The palette the environment is asking for.
    pub color_scheme: ColorScheme,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: ColorScheme::Light,
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
    style_document_with(document, viewport, &StyleSources::default())
}

/// Style every element in `document`, with the stylesheets its `<link>` elements
/// and their `@import`s asked for already fetched.
///
/// The fetch is the caller's: styling is synchronous and must not wait on a
/// network, so what arrives here is text that has already been got. A link with
/// nothing fetched for it simply contributes nothing, which is what a browser does
/// with a stylesheet that failed to load.
pub fn style_document_with(
    document: &Document,
    viewport: Viewport,
    sources: &StyleSources,
) -> StyledDocument {
    Styler::new(document, viewport, sources).style(document)
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
    /// The document's base URL, which a `style` attribute and a presentational
    /// hint resolve their addresses against: they are parsed afresh at every
    /// restyle, and each time against the same base as the `<style>` elements.
    base: UrlExtraData,
    /// Which rule each declaration block came from, by the block's address.
    ///
    /// The cascade hands back *what* applied — a chain of declaration blocks in
    /// the order they won — and never *what it was written as*, because a block
    /// does not know its own selector. So the selector is recorded here as the
    /// sheets are parsed, and looked up by the identity of the block itself. It
    /// is the one thing an inspector needs that the resolution does not carry.
    selectors: HashMap<usize, RuleSource>,
    /// The `@font-face` rules the page's sheets declare, in the order they were
    /// parsed. A page cannot be shown in a font it has not fetched, and nothing
    /// else on the way through knows the rules are there.
    font_faces: Vec<FontFace>,
}

/// A `@font-face` rule: a family the page brings with it, and where from.
///
/// The addresses are in the order the rule lists them, which is the order they
/// are to be tried in, and already absolute: each was resolved against the sheet
/// the rule was written in as that sheet was parsed (CSS Values 4 §4.5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFace {
    /// The family name the rule defines, as written.
    pub family: String,
    /// Every `url()` in its `src` that resolved, in order. A `local()` source
    /// names an installed family and is not an address, so it is not one of
    /// these.
    pub sources: Vec<Url>,
}

/// Where a declaration block was written.
#[derive(Clone, Debug)]
struct RuleSource {
    selector: String,
    origin: Origin,
}

/// One rule that applied to an element, as the inspector shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchedRule {
    /// The selector it was written with, or a stand-in for a block that has no
    /// selector — a `style` attribute has none.
    pub selector: String,
    /// Which sheet it came from: the browser's own, or the page's.
    pub origin: &'static str,
    /// The declarations in the block, in the order they were written.
    pub declarations: Vec<Declaration>,
}

/// One declaration inside a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The property's name, as CSS spells it.
    pub name: String,
    /// Its value, as CSS spells it.
    pub value: String,
    /// Whether it carries `!important`.
    pub important: bool,
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
    pub fn new(document: &Document, viewport: Viewport, sources: &StyleSources) -> Self {
        let _span = tracing::info_span!("parse_stylesheets").entered();
        enable_features();

        let lock = SharedRwLock::new();
        let quirks_mode = match document.quirks_mode() {
            html5ever::interface::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
            html5ever::interface::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
            html5ever::interface::QuirksMode::Quirks => QuirksMode::Quirks,
        };

        let mut stylist = Stylist::new(device_for(viewport, quirks_mode), quirks_mode);
        let mut selectors = HashMap::new();

        // The quirks rules after the ordinary ones, so that at equal specificity
        // they win; limited quirks mode takes none of them.
        let user_agent_sheets = match quirks_mode {
            QuirksMode::Quirks => &[UA_STYLESHEET, QUIRKS_STYLESHEET][..],
            QuirksMode::LimitedQuirks | QuirksMode::NoQuirks => &[UA_STYLESHEET][..],
        };
        for source in user_agent_sheets {
            let sheet = Arc::new(user_agent_sheet(source, &lock, quirks_mode));
            index_selectors(&sheet, Origin::UserAgent, &lock, &mut selectors);
            stylist.append_stylesheet(DocumentStyleSheet(sheet), &lock.read());
        }

        let imported = Cell::new(0);
        let parse = AuthorParse {
            lock: &lock,
            quirks_mode,
            imports: &sources.imports,
            imported: &imported,
        };
        let mut font_faces = Vec::new();
        for author in author_stylesheets(document, sources) {
            let media = parse.media(author.media.unwrap_or_default());
            let sheet = Arc::new(parse.sheet(
                &author.text,
                author.fetched_from.unwrap_or(&sources.base),
                media,
                author.fetched_from.into_iter().cloned().collect(),
            ));
            index_selectors(&sheet, Origin::Author, &lock, &mut selectors);
            collect_font_faces(&sheet, &lock, &mut font_faces);
            stylist.append_stylesheet(DocumentStyleSheet(sheet), &lock.read());
        }

        Self {
            lock,
            stylist,
            quirks_mode,
            viewport,
            base: UrlExtraData(Arc::new(sources.base.clone())),
            selectors,
            font_faces,
        }
    }

    /// The `@font-face` rules the page declares.
    pub fn font_faces(&self) -> &[FontFace] {
        &self.font_faces
    }

    /// Whether an author rule has taken a widget's own look away from it.
    ///
    /// CSS calls the result a *devolved* widget: a control the page has given a
    /// background, a border or a corner radius of its own is no longer drawn as a
    /// widget at all, because the page has said what it should look like and a
    /// theme drawn over that would be drawing over the answer. The list of
    /// properties is the specification's, and it is a list rather than "any
    /// declaration" — a page that sets a control's `color` still gets a widget.
    ///
    /// Asked once per control rather than once per element: the walk is the rule
    /// chain the cascade already built, and a page has a handful of controls.
    pub fn devolves(&self, style: &ComputedValues) -> bool {
        use style::properties::LonghandId;

        /// The properties that take a widget's look away. From the specification,
        /// in its order.
        const DISABLES: &[LonghandId] = &[
            LonghandId::BackgroundColor,
            LonghandId::BackgroundImage,
            LonghandId::BackgroundAttachment,
            LonghandId::BackgroundPositionX,
            LonghandId::BackgroundPositionY,
            LonghandId::BackgroundClip,
            LonghandId::BackgroundOrigin,
            LonghandId::BackgroundSize,
            LonghandId::BorderTopColor,
            LonghandId::BorderRightColor,
            LonghandId::BorderBottomColor,
            LonghandId::BorderLeftColor,
            LonghandId::BorderTopStyle,
            LonghandId::BorderRightStyle,
            LonghandId::BorderBottomStyle,
            LonghandId::BorderLeftStyle,
            LonghandId::BorderTopWidth,
            LonghandId::BorderRightWidth,
            LonghandId::BorderBottomWidth,
            LonghandId::BorderLeftWidth,
            LonghandId::BorderImageSource,
            LonghandId::BorderImageSlice,
            LonghandId::BorderImageWidth,
            LonghandId::BorderImageOutset,
            LonghandId::BorderImageRepeat,
            LonghandId::BorderTopLeftRadius,
            LonghandId::BorderTopRightRadius,
            LonghandId::BorderBottomRightRadius,
            LonghandId::BorderBottomLeftRadius,
        ];

        let guard = self.lock.read();
        for node in style.rules().self_and_ancestors() {
            let Some(source) = node.style_source() else {
                continue;
            };
            let key = source.get().raw_ptr().as_ptr() as usize;
            // A block with no recorded selector was not written as a rule: it is a
            // `style` attribute or a presentational hint, and both are the
            // author's.
            let author = self
                .selectors
                .get(&key)
                .is_none_or(|source| source.origin == Origin::Author);
            if !author {
                continue;
            }
            let block = source.read(&guard);
            for (declaration, _) in block.declaration_importance_iter() {
                if let style::properties::PropertyDeclarationId::Longhand(id) = declaration.id()
                    && DISABLES.contains(&id)
                {
                    return true;
                }
            }
        }
        false
    }

    /// The rules that produced a computed style, strongest last.
    ///
    /// Read off the rule node the cascade already built rather than by matching
    /// the selectors a second time: the chain hanging from a computed style *is*
    /// the list of declaration blocks that applied, in the order they won. A
    /// second matching pass would be a second answer to which rules apply, and
    /// the first time the two disagreed the pane would be the one lying.
    pub fn rules_for(&self, style: &ComputedValues) -> Vec<MatchedRule> {
        let guard = self.lock.read();
        let mut out = Vec::new();

        for node in style.rules().self_and_ancestors() {
            let Some(source) = node.style_source() else {
                continue;
            };
            let key = source.get().raw_ptr().as_ptr() as usize;
            let mut text = String::new();
            if source.read(&guard).to_css(&mut text).is_err() {
                continue;
            }
            // A block reaches the chain once for its normal declarations and
            // again for its important ones, because the cascade sorts those into
            // different levels. Each entry shows only the half it is: listing the
            // whole block twice would show a rule that is not there.
            let important = node.importance().important();
            let declarations: Vec<Declaration> = split_declarations(&text)
                .into_iter()
                .filter(|declaration| declaration.important == important)
                .collect();
            if declarations.is_empty() {
                continue;
            }

            // A block with no selector is one that was not written as a rule:
            // the markup's presentational hints, which cascade at a level of
            // their own, or a `style` attribute.
            let (selector, origin) = match self.selectors.get(&key) {
                Some(source) => (source.selector.clone(), origin_name(source.origin)),
                None if node.cascade_level().origin()
                    == style::rule_tree::CascadeOrigin::PresHints =>
                {
                    ("presentational hints".to_owned(), "attribute")
                }
                None => ("element.style".to_owned(), "attribute"),
            };
            out.push(MatchedRule {
                selector,
                origin,
                declarations,
            });
        }

        // `self_and_ancestors` walks from the winning end back; a stylesheet is
        // read the other way, weakest first, which is the order the cascade
        // resolved them in and the order a person expects to read them.
        out.reverse();
        out
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

    /// Whether any rule in the document depends on what a field holds.
    ///
    /// The states a typed character moves: whether the placeholder is showing,
    /// whether the value is valid, whether it is in range, and whether the reader
    /// has been told about either. A page with no such rule cannot restyle when a
    /// character is typed — which is what lets a field re-lay-out on its own
    /// instead of taking the document with it.
    ///
    /// Asked of the whole document rather than of the field, because the answer is
    /// the same for every field on the page and is wanted on every keystroke.
    pub fn value_changes_style(&mut self) -> bool {
        use stylo_dom::ElementState;

        {
            // The index is built as the sheets are flushed, so asking before the
            // first flush would ask an empty one.
            let guard = self.lock.read();
            self.stylist.flush(&StylesheetGuards::same(&guard));
        }
        crate::invalidation::document_depends_on(
            &self.stylist,
            ElementState::PLACEHOLDER_SHOWN
                | ElementState::VALUE_EMPTY
                | ElementState::VALID
                | ElementState::INVALID
                | ElementState::USER_VALID
                | ElementState::USER_INVALID
                | ElementState::INRANGE
                | ElementState::OUTOFRANGE,
        )
    }

    /// Whether moving the pointer or the focus from `before` to `after` can change
    /// any element's style.
    ///
    /// The answer is almost always no, and no costs a handful of intersections
    /// against an index the engine keeps anyway. Yes costs a restyle, which is what
    /// a page that styles its links on hover has asked for.
    pub fn interaction_changes_style(
        &mut self,
        document: &Document,
        form: &otlyra_dom::FormState,
        before: crate::state::Interaction,
        after: crate::state::Interaction,
    ) -> bool {
        if before == after {
            return false;
        }
        // The index is built as the sheets are flushed, so asking before the first
        // flush would ask an empty one.
        {
            let guard = self.lock.read();
            self.stylist.flush(&StylesheetGuards::same(&guard));
        }

        let before_states = crate::state::States::new(document, form, before);
        let after_states = crate::state::States::new(document, form, after);
        let touched = crate::state::touched_nodes(document, before, after);

        // The bucket lookup reads an element's id and classes through the matcher's
        // own atom table, which only exists in a slot — so slots are made, for the
        // touched elements and no others.
        let mut style_data = StyleData::with_lock(self.lock.clone());
        style_data.prepare_nodes(document, &touched, form, after, &self.base);
        let tree = Tree::styled(document, &style_data);
        let _scope = TreeScope::enter(&tree);
        touched.into_iter().any(|id| {
            let changed = before_states.state_of(id) ^ after_states.state_of(id);
            crate::invalidation::document_depends_on(&self.stylist, changed)
                && crate::invalidation::element_depends_on(&self.stylist, tree.node(id), changed)
        })
    }

    /// Compute a style for every element, for a page nobody is pointing at.
    pub fn style(&mut self, document: &Document) -> StyledDocument {
        self.style_with(
            document,
            &otlyra_dom::FormState::new(),
            crate::state::Interaction::none(),
        )
    }

    /// Compute a style for every element, given what the controls hold and where
    /// the pointer and the focus are.
    ///
    /// The two extra arguments are what makes `:checked` and `:hover` mean
    /// anything. They are passed rather than stored because neither belongs to the
    /// document: the same document in two windows has two pointers and one set of
    /// markup.
    pub fn style_with(
        &mut self,
        document: &Document,
        form: &otlyra_dom::FormState,
        interaction: crate::state::Interaction,
    ) -> StyledDocument {
        let _span = tracing::info_span!("recalc_style").entered();

        let mut style_data = StyleData::with_lock(self.lock.clone());
        style_data.prepare(document, form, interaction, &self.base);

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
        // Only a control can devolve, and a page has a handful of them — so the
        // question is asked here, where the rule chain is still to hand, rather
        // than left for whatever needs the answer to go looking for one.
        let devolved = styles
            .iter()
            .filter(|(node, style)| {
                otlyra_dom::form::Control::of(document, **node).is_some() && self.devolves(style)
            })
            .map(|(node, _)| *node)
            .collect();

        StyledDocument {
            style_data,
            styles,
            devolved,
        }
    }
}

/// Turn on the parts of the engine that ship switched off.
///
/// The style engine carries preferences from the browser it was taken from, and
/// some of them gate whether a value parses at all: with `layout.grid.enabled`
/// false, `display: grid` is not a display value and every grid on the web lays out
/// as a block. Set before the first stylesheet is parsed, because a value that did
/// not parse is not stored anywhere to be reconsidered.
///
/// `layout.unimplemented` is one switch for every property Servo parses before it
/// lays any of them out: `mask` and its longhands (and the `-webkit-` spellings
/// every page still writes), `backdrop-filter`, `text-overflow`, `counter-reset`
/// and `counter-increment`, `user-select`, `contain`, `color-scheme`, and a tail
/// of newer ones (`corner-shape`, `position-area`, `offset-path`, view
/// transitions, scroll-driven animation). Parsing one does not draw it — each is
/// read, or not, where it is used — but a declaration that was thrown away at
/// parse time can never be read at all. `zoom` sits behind the same switch and
/// stays off: Stylo enables it for the browser's own sheets only.
fn enable_features() {
    use std::sync::Once;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        stylo_static_prefs::set_pref!("layout.grid.enabled", true);
        stylo_static_prefs::set_pref!("layout.variable_fonts.enabled", true);
        stylo_static_prefs::set_pref!("layout.unimplemented", true);
    });
}

/// The initial font, with the default size multiplied by the reader's scale.
///
/// `computed_size` and `used_size` both, because they are the same value until
/// something constrains one of them and nothing here does. The keyword
/// information is left alone: `medium` is still what this is, and `font-size:
/// larger` on top of it still means larger *than this*.
fn scaled_font(scale: f32) -> style::properties::style_structs::Font {
    use style::values::computed::length::NonNegativeLength;

    let mut font = style::properties::style_structs::Font::initial_values();
    if (scale - 1.0).abs() < f32::EPSILON {
        return font;
    }
    let scaled = font.font_size.computed_size.0.px() * scale;
    font.font_size.computed_size = NonNegativeLength::new(scaled);
    font.font_size.used_size = NonNegativeLength::new(scaled);
    font
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
        // The initial values every cascade starts from, with the default font
        // size the reader's own preference rather than the specification's
        // sixteen pixels. This is the one place a browser's *default font size*
        // setting belongs: everything that does not name a size inherits from
        // here, and everything that names one overrides it, which is what makes
        // the preference a default rather than an override.
        ComputedValues::initial_values_with_font_override(scaled_font(viewport.text_scale)),
        match viewport.color_scheme {
            ColorScheme::Light => style::queries::values::PrefersColorScheme::Light,
            ColorScheme::Dark => style::queries::values::PrefersColorScheme::Dark,
        },
        DESKTOP_POINTER,
        DESKTOP_POINTER,
    )
}

/// What the pointer can do, for `pointer`, `hover` and their `any-` forms
/// (Media Queries 4, "Interaction Media Features"): a mouse, which points
/// precisely and hovers. It is both the primary pointer and the only one, so a
/// page that keeps its hover styles behind `(hover: hover)`, as Tailwind's do,
/// gets them.
const DESKTOP_POINTER: style::servo::media_features::PointerCapabilities =
    style::servo::media_features::PointerCapabilities::FINE
        .union(style::servo::media_features::PointerCapabilities::HOVER);

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

        // `rem` is the *root element's* font size, and the engine reads it from
        // the device rather than from the tree — so somebody has to put it there
        // once the root has been cascaded. Servo does it inside the traversal it
        // owns; we drive the resolver ourselves, so it happens here.
        //
        // Left unset it stays at sixteen pixels, and a page that sets its own
        // root size — `html { font-size: 1.25em }`, which is most of the modern
        // web — has every length it wrote in `rem` come out a fifth too small.
        // That is not a subtle wrong: it is every padding, every radius and every
        // font on the page, and it looks like a browser that cannot lay out.
        //
        // Before the children, because the root's size is what they are resolved
        // against. The root's own `rem` lengths were computed a line ago against
        // whatever it was before, which is what the specification says for
        // `font-size` on the root and near enough for the rest.
        if depth == 0 {
            let device = context.shared.stylist.device();
            let size = style.get_font().clone_font_size().computed_size();
            device.set_root_font_size(style.effective_zoom.unzoom(size.px()));
            device.set_root_style(&style);
        }

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
fn author_stylesheets<'a>(
    document: &'a Document,
    sources: &'a StyleSources,
) -> Vec<AuthorSheet<'a>> {
    let mut sheets = Vec::new();
    let mut stack = vec![document.root()];

    while let Some(id) = stack.pop() {
        if let Some(element) = document.get(id).and_then(|node| node.element()) {
            let media = element.attr("media");
            match element.name.local.as_ref() {
                "style" => {
                    let text = style_text(document, id);
                    if !text.trim().is_empty() {
                        sheets.push(AuthorSheet {
                            text: Cow::Owned(text),
                            fetched_from: None,
                            media,
                        });
                    }
                }
                "link" => {
                    if let Some(fetched) = sources.links.get(&id) {
                        sheets.push(AuthorSheet {
                            text: Cow::Borrowed(&fetched.text),
                            fetched_from: Some(&fetched.url),
                            media,
                        });
                    }
                }
                _ => {}
            }
        }
        stack.extend(document.children(id).collect::<Vec<_>>().into_iter().rev());
    }

    sheets
}

/// What a `<style>` element says: its child text, in order.
fn style_text(document: &Document, style: NodeId) -> String {
    let mut text = String::new();
    for child in document.children(style) {
        if let Some(NodeData::Text(chunk)) = document.get(child).map(|node| &node.data) {
            text.push_str(chunk);
        }
    }
    text
}

/// Every `@import` in the document's own `<style>` elements, resolved against
/// its base, with the element each is in — the first of the two passes
/// [`imports_in`] describes, for the sheets the document holds rather than
/// fetches.
pub fn style_element_imports(document: &Document, base: &Url) -> Vec<(NodeId, Url)> {
    let mut found = Vec::new();
    let mut stack = vec![document.root()];
    while let Some(id) = stack.pop() {
        if let Some(element) = document.get(id).and_then(|node| node.element())
            && element.name.local.as_ref() == "style"
        {
            let imports = imports_in(&style_text(document, id), base);
            found.extend(imports.into_iter().map(|url| (id, url)));
        }
        stack.extend(document.children(id).collect::<Vec<_>>().into_iter().rev());
    }
    found
}

/// One author stylesheet in the document, as the cascade takes it.
struct AuthorSheet<'a> {
    /// What the sheet says.
    text: Cow<'a, str>,
    /// Where a linked sheet was served from, which is what its relative
    /// addresses resolve against. A `<style>` is at no address of its own, and
    /// resolves them against the document's base.
    fetched_from: Option<&'a Url>,
    /// Its element's `media` attribute, which applies to the whole sheet — a
    /// sheet written for print styles nothing on a screen (HTML §4.2.4, §4.2.6).
    media: Option<&'a str>,
}

/// What a document's style is made of besides the document: the address its
/// relative URLs resolve against, and the stylesheets fetched for it.
///
/// The fetches are the caller's, and are done before the cascade runs; this is
/// what they came to.
#[derive(Clone, Debug)]
pub struct StyleSources {
    /// The document base URL (HTML §2.4.1): what a `<style>` element, a `style`
    /// attribute and a presentational hint resolve their addresses against.
    pub base: Url,
    /// The sheet each `<link rel=stylesheet>` fetched, by the element.
    pub links: HashMap<NodeId, FetchedSheet>,
    /// Every sheet an `@import` fetched, by the address the rule named once
    /// resolved against the sheet it is in.
    pub imports: HashMap<Url, FetchedSheet>,
}

impl Default for StyleSources {
    /// A document at `about:blank` with nothing fetched for it: what a document
    /// made from a string is.
    fn default() -> Self {
        Self {
            base: about_blank(),
            links: HashMap::new(),
            imports: HashMap::new(),
        }
    }
}

/// A stylesheet as it came off the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedSheet {
    /// The address the response finally came from, after any redirect: the
    /// sheet's own URL, which its relative addresses resolve against (CSS
    /// Values 4 §4.5.1).
    pub url: Url,
    /// Its text, decoded.
    pub text: String,
}

/// The address of a document that came from nowhere, and so the base of one: a
/// relative URL resolves against it to nothing.
pub fn about_blank() -> Url {
    Url::parse("about:blank").expect("about:blank parses")
}

/// How many imported sheets one [`Styler::new`] parses, at most.
///
/// What a page may fetch is bounded where it is fetched — so many sheets, so
/// deep — but a sheet fetched once can be imported any number of times, and
/// every `@import` is a sheet of its own in the cascade (CSS Cascade 5 §2.1),
/// parsed and walked afresh. Four sheets that each import the next a dozen
/// times over are twenty thousand parses on the thread that draws. So past
/// this many an `@import` is refused, as one whose fetch failed would be: four
/// times the thirty-two sheets the browser fetches for a page, which no page
/// written by hand comes near. The limit is ours; the specification sets none.
const IMPORTED_SHEET_LIMIT: usize = 128;

/// How the page's own stylesheets are parsed, shared by the sheets the document
/// names and the sheets those import.
#[derive(Clone, Copy)]
struct AuthorParse<'a> {
    /// The lock every rule in the document is kept under.
    lock: &'a SharedRwLock,
    quirks_mode: QuirksMode,
    /// The sheets the document's `@import` rules fetched.
    imports: &'a HashMap<Url, FetchedSheet>,
    /// How many imported sheets have been parsed so far, counted against
    /// [`IMPORTED_SHEET_LIMIT`].
    imported: &'a Cell<usize>,
}

impl AuthorParse<'_> {
    /// Parse one author sheet at `url`, for `media`.
    ///
    /// `chain` is the sheets it is being imported through, outermost first, so
    /// that an import of one of them is a cycle and is refused rather than
    /// parsed for ever.
    fn sheet(
        &self,
        text: &str,
        url: &Url,
        media: Arc<Locked<MediaList>>,
        chain: Vec<Url>,
    ) -> Stylesheet {
        // `appearance` has to survive the parser, and the parser has no such
        // property; see the module that carries it for why the name is changed
        // rather than the meaning.
        let text = crate::appearance::rewrite_stylesheet(text);
        let loader = ImportLoader {
            parse: *self,
            chain,
        };
        Stylesheet::from_str(
            &text,
            UrlExtraData(Arc::new(url.clone())),
            Origin::Author,
            media,
            self.lock.clone(),
            Some(&loader),
            None,
            self.quirks_mode,
            AllowImportRules::Yes,
        )
    }

    /// A `media` attribute as the media list its sheet applies for.
    ///
    /// Parsed into the list the sheet carries rather than wrapped around the
    /// text as an `@media` block: the engine evaluates the one exactly as it
    /// does a block, and a stray `}` in the sheet cannot close it.
    fn media(&self, text: &str) -> Arc<Locked<MediaList>> {
        Arc::new(self.lock.wrap(media_list(text, self.quirks_mode)))
    }
}

/// What the cascade answers an `@import` with: the sheet fetched for it, parsed
/// in its place, or a refusal (CSS Cascade 5 §2).
///
/// The engine applies what the rule says about the sheet it gets — the media
/// it is for, the layer it goes in and where it cascades — so this decides
/// only which sheet that is.
struct ImportLoader<'a> {
    parse: AuthorParse<'a>,
    /// The sheets being parsed, outermost first, by every address each is known
    /// by.
    chain: Vec<Url>,
}

impl StylesheetLoader for ImportLoader<'_> {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: cssparser::SourceLocation,
        lock: &SharedRwLock,
        media: Arc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> Arc<Locked<ImportRule>> {
        let stylesheet = self.imported(&url, media, supports.as_ref());
        Arc::new(lock.wrap(ImportRule {
            url,
            stylesheet,
            supports,
            layer,
            source_location: location,
        }))
    }
}

impl ImportLoader<'_> {
    /// The sheet an `@import` of `url` brings in.
    ///
    /// Refused when there is none to bring: the address does not resolve, the
    /// fetch failed or was never made, or the sheet is one of those importing
    /// it. Refused too when its `supports()` is false, which the engine does
    /// not ask when it cascades — so the sheet is never looked at, which is
    /// what the specification allows and what Gecko does — and once
    /// [`IMPORTED_SHEET_LIMIT`] sheets have been imported.
    fn imported(
        &self,
        url: &CssUrl,
        media: Arc<Locked<MediaList>>,
        supports: Option<&ImportSupportsCondition>,
    ) -> ImportSheet {
        if supports.is_some_and(|condition| !condition.enabled) {
            return ImportSheet::new_refused();
        }
        let Some(target) = url.url() else {
            return ImportSheet::new_refused();
        };
        let Some(fetched) = self.parse.imports.get(&**target) else {
            return ImportSheet::new_refused();
        };
        let cycle = self
            .chain
            .iter()
            .any(|parsing| parsing == &**target || *parsing == fetched.url);
        if cycle {
            return ImportSheet::new_refused();
        }
        let imported = self.parse.imported.get();
        if imported >= IMPORTED_SHEET_LIMIT {
            tracing::warn!(url = %fetched.url, "an import past the limit is not parsed");
            return ImportSheet::new_refused();
        }
        self.parse.imported.set(imported + 1);

        let mut chain = self.chain.clone();
        chain.extend([(**target).clone(), fetched.url.clone()]);
        ImportSheet::new(Arc::new(self.parse.sheet(
            &fetched.text,
            &fetched.url,
            media,
            chain,
        )))
    }
}

/// The sheets a stylesheet's `@import` rules ask for, in the order it names
/// them, resolved against `url` — the sheet's own address, or the document's
/// base for a `<style>`.
///
/// The first of two passes (CSS Cascade 5 §2). The cascade answers an
/// `@import` from sheets already fetched, and the fetching needs the addresses
/// first; so the sheet is parsed once here for nothing but those, and again when
/// the page is styled. A rule whose `supports()` is false asks for nothing — the
/// cascade would refuse it — and one for another medium is still fetched,
/// because a resize can make it apply.
pub fn imports_in(text: &str, url: &Url) -> Vec<Url> {
    /// A loader that fetches nothing and notes what it was asked for.
    #[derive(Default)]
    struct Recorder(RefCell<Vec<Url>>);

    impl StylesheetLoader for Recorder {
        fn request_stylesheet(
            &self,
            url: CssUrl,
            location: cssparser::SourceLocation,
            lock: &SharedRwLock,
            _media: Arc<Locked<MediaList>>,
            supports: Option<ImportSupportsCondition>,
            layer: ImportLayer,
        ) -> Arc<Locked<ImportRule>> {
            let wanted = supports.as_ref().is_none_or(|condition| condition.enabled);
            if wanted && let Some(target) = url.url() {
                self.0.borrow_mut().push((**target).clone());
            }
            Arc::new(lock.wrap(ImportRule {
                url,
                stylesheet: ImportSheet::new_pending(),
                supports,
                layer,
                source_location: location,
            }))
        }
    }

    enable_features();
    let lock = SharedRwLock::new();
    let recorder = Recorder::default();
    Stylesheet::from_str(
        text,
        UrlExtraData(Arc::new(url.clone())),
        Origin::Author,
        Arc::new(lock.wrap(MediaList::empty())),
        lock.clone(),
        Some(&recorder),
        None,
        // Nothing an `@import` says reads differently in quirks mode.
        QuirksMode::NoQuirks,
        AllowImportRules::Yes,
    );
    recorder.0.into_inner()
}

/// A stylesheet a document asks for but does not contain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StylesheetLink {
    /// The `<link>` element, which is how the fetched text finds its way back to
    /// the place in the document that decides where it cascades.
    pub node: NodeId,
    /// The address, exactly as the attribute spells it — resolving it against the
    /// document's own needs the document's address, which this crate does not know.
    pub href: String,
    /// What the link says it is for, if it says. Empty means every medium, which
    /// is what a `<link>` without the attribute means.
    pub media: String,
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
            let rel = document.attr(id, "rel").unwrap_or_default();
            let mut keywords = rel.split_ascii_whitespace().map(str::to_ascii_lowercase);
            let is_sheet = keywords.clone().any(|word| word == "stylesheet");
            let alternate = keywords.any(|word| word == "alternate");

            if is_sheet
                && !alternate
                && let Some(href) = document.attr(id, "href")
                && !href.trim().is_empty()
            {
                let media = document.attr(id, "media").unwrap_or_default();
                links.push(StylesheetLink {
                    node: id,
                    href: href.to_owned(),
                    media: media.to_owned(),
                });
            }
        }
        stack.extend(document.children(id).collect::<Vec<_>>().into_iter().rev());
    }

    links
}

/// Whether a media condition written outside a stylesheet matches `viewport`.
///
/// `<source media>` and the conditions in an `<img sizes>` are media queries in
/// an attribute rather than in a sheet, and they have to be answered the same way
/// `@media` is or a page gets one answer in its CSS and another in its markup.
/// The engine's own parser and device do it; nothing here re-implements matching.
pub fn media_condition_matches(condition: &str, viewport: Viewport) -> bool {
    enable_features();
    let device = device_for(viewport, QuirksMode::NoQuirks);
    media_list(condition, QuirksMode::NoQuirks).evaluate(
        &device,
        QuirksMode::NoQuirks,
        // A `@custom-media` name is defined in a stylesheet, and this condition
        // is not in one.
        &mut style::stylesheets::CustomMediaEvaluator::none(),
    )
}

/// A media query list written outside a sheet: an element's `media`, or a
/// condition in an attribute. Empty, or nothing but whitespace, is every medium.
fn media_list(text: &str, quirks_mode: QuirksMode) -> MediaList {
    // A media query has no address in it, and the parser wants a base anyway.
    let url = UrlExtraData(Arc::new(about_blank()));
    let context = style::parser::ParserContext::new(
        Origin::Author,
        &url,
        None,
        style_traits::ParsingMode::DEFAULT,
        quirks_mode,
        Default::default(),
        None,
        None,
        Default::default(),
    );
    let mut input = cssparser::ParserInput::new(text);
    MediaList::parse(&context, &mut cssparser::Parser::new(&mut input))
}

/// Parse one of the browser's own stylesheets.
///
/// At no address, because there is nothing in one to resolve.
fn user_agent_sheet(source: &str, lock: &SharedRwLock, quirks_mode: QuirksMode) -> Stylesheet {
    Stylesheet::from_str(
        source,
        UrlExtraData(Arc::new(about_blank())),
        Origin::UserAgent,
        Arc::new(lock.wrap(MediaList::empty())),
        lock.clone(),
        None,
        None,
        quirks_mode,
        AllowImportRules::No,
    )
}

/// One serialized declaration block, taken apart into its declarations.
///
/// The engine serializes a block the way CSSOM says to — `name: value` joined by
/// `; ` — and offers no way to ask for one declaration at a time that does not
/// want a `Stylist` and a block of its own. So the block is spelled once and cut
/// here, at the semicolons that are not inside brackets or quotes: a `url(a;b)`
/// or a `content: ";"` is one value and not two declarations.
fn split_declarations(text: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);

    let mut push = |piece: &str| {
        let piece = piece.trim();
        if piece.is_empty() {
            return;
        }
        let Some((name, value)) = piece.split_once(':') else {
            return;
        };
        let value = value.trim();
        let (value, important) = match value.strip_suffix("!important") {
            Some(rest) => (rest.trim_end(), true),
            None => (value, false),
        };
        out.push(Declaration {
            name: name.trim().to_owned(),
            value: value.to_owned(),
            important,
        });
    };

    for (at, character) in text.char_indices() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ';') if depth == 0 => {
                push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    push(&text[start..]);
    out
}

/// What an origin is called where a person reads it.
fn origin_name(origin: Origin) -> &'static str {
    match origin {
        Origin::UserAgent => "browser",
        Origin::User => "user",
        Origin::Author => "page",
    }
}

/// Record the selector every style rule in `sheet` was written with.
///
/// Keyed by the address of the declaration block, because that is the only thing
/// the cascade hands back later. Nested rules are walked too, so a declaration
/// inside a media query, a nesting block or an imported sheet is named by the
/// selector it was actually written under rather than dropped for having no
/// top-level one.
fn index_selectors(
    sheet: &Stylesheet,
    origin: Origin,
    lock: &SharedRwLock,
    out: &mut HashMap<usize, RuleSource>,
) {
    use style::stylesheets::CssRule;

    let guard = lock.read();
    let mut stack: Vec<Arc<style::shared_lock::Locked<style::stylesheets::CssRules>>> =
        vec![sheet.contents.read_with(&guard).rules.clone()];

    while let Some(rules) = stack.pop() {
        for rule in &rules.read_with(&guard).0 {
            match rule {
                CssRule::Style(rule) => {
                    let rule = rule.read_with(&guard);
                    let key = rule.block.raw_ptr().as_ptr() as usize;
                    out.entry(key).or_insert_with(|| RuleSource {
                        selector: {
                            use cssparser::ToCss as _;
                            rule.selectors.to_css_string()
                        },
                        origin,
                    });
                    if let Some(nested) = rule.rules.as_ref() {
                        stack.push(nested.clone());
                    }
                }
                CssRule::Media(rule) => stack.push(rule.rules.clone()),
                CssRule::Supports(rule) => stack.push(rule.rules.clone()),
                CssRule::Import(rule) => stack.extend(imported_rules(rule, &guard)),
                _ => {}
            }
        }
    }
}

/// Collect the `@font-face` rules in one sheet, and in the sheets it imports.
///
/// Nested in the same way style rules are: a rule inside a media query counts, and
/// counts whether or not the query matches — which sheet it came in gates that,
/// and a rule that turns out not to apply costs a fetch nobody uses rather than a
/// page set in the wrong font.
fn collect_font_faces(sheet: &Stylesheet, lock: &SharedRwLock, out: &mut Vec<FontFace>) {
    use style::stylesheets::CssRule;

    let guard = lock.read();
    let mut stack: Vec<Arc<style::shared_lock::Locked<style::stylesheets::CssRules>>> =
        vec![sheet.contents.read_with(&guard).rules.clone()];

    while let Some(rules) = stack.pop() {
        for rule in &rules.read_with(&guard).0 {
            match rule {
                CssRule::FontFace(rule) => {
                    let descriptors = &rule.read_with(&guard).descriptors;
                    let (Some(family), Some(sources)) =
                        (descriptors.font_family.as_ref(), descriptors.src.as_ref())
                    else {
                        continue;
                    };
                    let sources: Vec<Url> = sources
                        .0
                        .iter()
                        .filter_map(|source| match source {
                            style::font_face::Source::Url(source) if readable(source) => {
                                source.url.url().map(|url| (**url).clone())
                            }
                            style::font_face::Source::Url(_)
                            | style::font_face::Source::Local(_) => None,
                        })
                        .collect();
                    if sources.is_empty() {
                        continue;
                    }
                    out.push(FontFace {
                        family: family.name.to_string(),
                        sources,
                    });
                }
                CssRule::Media(rule) => stack.push(rule.rules.clone()),
                CssRule::Supports(rule) => stack.push(rule.rules.clone()),
                CssRule::Import(rule) => stack.extend(imported_rules(rule, &guard)),
                _ => {}
            }
        }
    }
}

/// Whether a `src` entry names a format worth fetching.
///
/// A page that still supports browsers from before the web had a font format
/// lists several: an EOT for one of them, an SVG font for another, and a WOFF2
/// for everything since. Taking the first of those fetches a file nothing can
/// read; every browser picks by the `format()` hint, and by the address when the
/// page did not write one.
fn readable(source: &style::font_face::UrlSource) -> bool {
    use style::font_face::{FontFaceSourceFormat, FontFaceSourceFormatKeyword as Keyword};

    if let Some(hint) = source.format_hint.as_ref() {
        return match hint {
            FontFaceSourceFormat::Keyword(keyword) => !matches!(
                keyword,
                Keyword::EmbeddedOpentype | Keyword::Svg | Keyword::None
            ),
            FontFaceSourceFormat::String(name) => {
                let name = name.to_ascii_lowercase();
                !(name.contains("embedded-opentype") || name.contains("svg"))
            }
        };
    }

    // No hint: the address is what is left to go on, and the two formats worth
    // refusing are the two nothing here can read.
    let path = source
        .url
        .url()
        .map(|url| url.path().to_ascii_lowercase())
        .unwrap_or_default();
    !(path.ends_with(".eot") || path.ends_with(".svg"))
}

/// The rules of the sheet an `@import` brought in, if it brought one in.
fn imported_rules(
    rule: &Locked<ImportRule>,
    guard: &style::shared_lock::SharedRwLockReadGuard,
) -> Option<Arc<Locked<style::stylesheets::CssRules>>> {
    let sheet = rule.read_with(guard).stylesheet.as_sheet()?;
    Some(sheet.contents.read_with(guard).rules.clone())
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

    /// What `font-size: medium` is, which is where every other keyword and every
    /// unstyled element starts from.
    ///
    /// Monospace gets its own, smaller, base — the same thirteen pixels every
    /// browser uses. A monospace face at the same size as the prose around it reads
    /// as larger than it, because its letters are all as wide as its widest, so
    /// `<code>` in a paragraph would stand out by size rather than by shape. The
    /// keyword is what carries it: an element whose family is a single generic
    /// resolves `medium` against that generic's base, and a size written in `em`
    /// keeps the chain, so `<code>` inside a heading is scaled by the heading's own
    /// ratio rather than pinned at thirteen.
    fn base_size_for_generic(
        &self,
        generic: style::values::computed::font::GenericFontFamily,
    ) -> style::values::computed::Length {
        use style::values::computed::font::GenericFontFamily;

        style::values::computed::Length::new(match generic {
            GenericFontFamily::Monospace => 13.0,
            _ => 16.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Style a document and hand back the computed values of the first element
    /// matching `selector`.
    fn computed(html: &str, selector: &str) -> Arc<ComputedValues> {
        styled(html, |_| StyleSources::default(), selector)
    }

    /// Style `html` with the sources `sources` makes for its parsed document,
    /// and read the first element matching `selector` back.
    fn styled(
        html: &str,
        sources: impl FnOnce(&Document) -> StyleSources,
        selector: &str,
    ) -> Arc<ComputedValues> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styled = style_document_with(&document, Viewport::default(), &sources(&document));
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

    /// What a document at `base` is styled with when nothing was fetched for it.
    fn at(base: &str) -> StyleSources {
        StyleSources {
            base: Url::parse(base).expect("a test's base parses"),
            ..StyleSources::default()
        }
    }

    /// A fetched sheet: its address and its text.
    fn fetched(url: &str, text: &str) -> FetchedSheet {
        FetchedSheet {
            url: Url::parse(url).expect("a test's address parses"),
            text: text.to_owned(),
        }
    }

    /// Where a `@font-face` rule says its font can be fetched from, as text.
    fn addresses(face: &FontFace) -> Vec<&str> {
        face.sources.iter().map(Url::as_str).collect()
    }

    /// One fetched sheet per `<link>`, in the order the links appear, each at the
    /// address it is paired with, in a document at `https://x.test/`.
    fn linked(document: &Document, sheets: &[(impl AsRef<str>, &str)]) -> StyleSources {
        let links = stylesheet_links(document);
        assert_eq!(links.len(), sheets.len(), "one sheet per link");
        StyleSources {
            links: links
                .iter()
                .zip(sheets)
                .map(|(link, (url, text))| (link.node, fetched(url.as_ref(), text)))
                .collect(),
            ..at("https://x.test/")
        }
    }

    /// Style a document with one fetched sheet per `<link>`, in the order the
    /// links appear, and read one element's computed style back.
    fn computed_with_links(html: &str, sheets: &[&str], selector: &str) -> Arc<ComputedValues> {
        let numbered: Vec<(String, &str)> = sheets
            .iter()
            .enumerate()
            .map(|(at, text)| (format!("https://x.test/sheet{at}.css"), *text))
            .collect();
        styled(html, |document| linked(document, &numbered), selector)
    }

    /// The rules that decide whether an element renders like itself, checked
    /// against the reference's own answers for the ones that were wrong.
    #[test]
    fn the_built_in_sheet_says_what_a_browser_says() {
        use crate::Display;

        let display = |markup: &str, selector: &str| {
            let document = otlyra_html::parse(markup.as_bytes(), Some("utf-8")).document;
            let styled = style_document(&document, Viewport::default());
            let node = crate::stylo_dom::select(&document, selector)
                .expect("the selector parses")
                .into_iter()
                .next()
                .expect("something matches");
            styled
                .style_of(node)
                .map(|values| crate::computed::to_layout_style(values).display)
        };

        // Not shown at all: the attribute that says so, a field that is not one,
        // a list of suggestions, the parenthesis a browser with ruby hides, what a
        // browser without frames or plug-ins would show, and a popover nothing
        // has opened.
        for (markup, selector) in [
            ("<p hidden>x", "p"),
            ("<input type=hidden>", "input"),
            ("<input type=HIDDEN style=display:block>", "input"),
            ("<datalist><option>x</option></datalist>", "datalist"),
            ("<ruby>x<rp>(</rp></ruby>", "rp"),
            ("<dialog>x</dialog>", "dialog"),
            ("<noframes>x</noframes>", "noframes"),
            ("<noembed>x</noembed>", "noembed"),
            ("<object><param name=a value=b></object>", "param"),
            ("<map><area href=/x></map>", "area"),
            ("<div popover>x</div>", "div"),
            ("<div popover=manual>x</div>", "div"),
            // An `audio` with no controls, whatever the page says.
            ("<audio></audio>", "audio"),
            ("<audio style=display:block></audio>", "audio"),
            // And what SVG never draws where it is written.
            ("<svg><title>Logo</title></svg>", "title"),
            (
                "<svg><linearGradient></linearGradient></svg>",
                "linearGradient",
            ),
        ] {
            assert_eq!(
                display(markup, selector),
                Some(Display::None),
                "{selector} in {markup:?} should not be rendered"
            );
        }

        // The rules are HTML's alone: an SVG element is drawn whatever its
        // `hidden` says, as it is in both references.
        assert_eq!(
            display("<svg hidden width=10 height=10></svg>", "svg"),
            Some(Display::Inline)
        );

        // An `audio` with controls is shown, and a hidden `embed` keeps a box,
        // of no size, because it still loads what it names (§15.3.1).
        assert_eq!(
            display("<audio controls></audio>", "audio"),
            Some(Display::Inline)
        );
        assert_eq!(
            display("<embed hidden src=a.swf>", "embed"),
            Some(Display::Inline)
        );

        // The element is an inline like any unknown one; what is in it is left out
        // by the box builder, because scripts run.
        assert_eq!(
            display("<noscript>x</noscript>", "noscript"),
            Some(Display::Inline)
        );

        // A control is inline outside and a block inside.
        for selector in ["input", "button", "select", "textarea", "progress"] {
            let markup = format!("<{selector}>x</{selector}>");
            assert_eq!(
                display(&markup, selector),
                Some(Display::InlineBlock),
                "{selector} is inline-block"
            );
        }

        // A closed disclosure shows its summary and nothing else; an open one
        // shows both.
        assert_eq!(
            display("<details><summary>s</summary><p>body</p></details>", "p"),
            Some(Display::None)
        );
        assert_eq!(
            display(
                "<details open><summary>s</summary><p>body</p></details>",
                "p"
            ),
            Some(Display::Block)
        );

        // An anchor without an address is a name for a place, not a link.
        let styled_link = |markup: &str| {
            let document = otlyra_html::parse(markup.as_bytes(), Some("utf-8")).document;
            let styled = style_document(&document, Viewport::default());
            let node = crate::stylo_dom::select(&document, "a")
                .expect("parses")
                .into_iter()
                .next()
                .expect("an anchor");
            crate::computed::to_layout_style(styled.style_of(node).expect("styled"))
                .text_decoration
                .underline
        };
        assert!(styled_link("<a href=/x>x</a>"), "a link is underlined");
        assert!(!styled_link("<a name=x>x</a>"), "an anchor is not");

        // `pre` from before there was a `pre`, and the line that does not break.
        let style = |markup: &str, selector: &str| {
            let document = otlyra_html::parse(markup.as_bytes(), Some("utf-8")).document;
            let styled = style_document(&document, Viewport::default());
            let node = crate::stylo_dom::select(&document, selector).expect("parses")[0];
            crate::computed::to_layout_style(styled.style_of(node).expect("styled"))
        };
        for (markup, selector) in [
            ("<xmp><b>x</b></xmp>", "xmp"),
            ("<listing>x</listing>", "listing"),
            ("<plaintext>x", "plaintext"),
        ] {
            let block = style(markup, selector);
            assert_eq!(block.display, Display::Block, "{selector}");
            assert_eq!(block.white_space, crate::WhiteSpace::Preserve, "{selector}");
            // An em of the thirteen pixels monospace is set in.
            assert_eq!(
                block.margin.top,
                crate::LengthOrAuto::Length(crate::Length::Px(13.0)),
                "{selector}"
            );
        }
        assert_eq!(
            style("<p><nobr>x y</nobr>", "nobr").text_wrap,
            crate::TextWrap::NoWrap
        );
        assert!(
            style("<p><abbr title=HyperText>HT</abbr>", "abbr")
                .text_decoration
                .underline,
            "an abbreviation with its expansion is underlined"
        );
        assert!(
            !style("<p><abbr>HT</abbr>", "abbr")
                .text_decoration
                .underline,
            "and one without is not"
        );
        assert_eq!(style("<p>x<br clear=all>y", "br").clear, crate::Clear::Both);
        // A frame inside MathML is MathML's, and HTML's inset edge is not drawn
        // round it.
        assert_eq!(
            style("<math><iframe></iframe></math>", "iframe")
                .border
                .top
                .width,
            0.0
        );
        assert_eq!(style("<iframe></iframe>", "iframe").border.top.width, 2.0);
    }

    /// An open `dialog` is shown whatever its `popover` attribute says: the rule
    /// that hides a popover nothing has opened is not among the ones that reach
    /// it.
    #[test]
    fn the_popover_rule_leaves_an_open_dialog_alone() {
        let hides_it = |markup: &str, selector: &str| {
            let document = otlyra_html::parse(markup.as_bytes(), Some("utf-8")).document;
            let mut styler = Styler::new(&document, Viewport::default(), &StyleSources::default());
            let styled = styler.style(&document);
            let node = crate::stylo_dom::select(&document, selector).expect("parses")[0];
            styler
                .rules_for(styled.style_of(node).expect("styled"))
                .iter()
                .any(|rule| rule.selector.starts_with("[popover]"))
        };
        assert!(hides_it("<div popover>x</div>", "div"));
        assert!(hides_it("<dialog popover>x</dialog>", "dialog"));
        assert!(!hides_it("<dialog open popover>x</dialog>", "dialog"));
    }

    /// The layout style of a document's first `td`.
    fn cell(html: &str) -> crate::ComputedStyle {
        crate::computed::to_layout_style(&computed(html, "td"))
    }

    /// A table in quirks mode takes nothing of the text around it: not the size
    /// of the font and not where the lines go. In standards mode, and in limited
    /// quirks mode, it inherits both like anything else.
    #[test]
    fn a_quirks_mode_table_resets_the_text_it_inherits() {
        let table = "<center style='font: 13px Verdana'><table><tr><td>x</table></center>";

        let quirks = cell(table);
        assert_eq!(quirks.font_size, 16.0);
        assert_eq!(quirks.text_align, crate::TextAlign::Start);

        for doctype in [
            "<!doctype html>",
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\" \
             \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd\">",
        ] {
            let standards = cell(&format!("{doctype}{table}"));
            assert_eq!(standards.font_size, 13.0, "{doctype}");
            assert_eq!(standards.text_align, crate::TextAlign::Center, "{doctype}");
        }
    }

    /// The rest of HTML's quirks rules: a form's bottom margin, and a field
    /// measured across its border.
    #[test]
    fn the_quirks_rules_apply_in_quirks_mode_alone() {
        let layout = |html: &str, selector: &str| {
            crate::computed::to_layout_style(&computed(html, selector))
        };
        let form = |doctype: &str| {
            layout(&format!("{doctype}<form>x</form>"), "form")
                .margin
                .bottom
        };
        assert_eq!(
            form(""),
            crate::LengthOrAuto::Length(crate::Length::Px(16.0))
        );
        assert_eq!(
            form("<!doctype html>"),
            crate::LengthOrAuto::Length(crate::Length::Px(0.0))
        );
        assert_eq!(
            layout("<input>", "input").box_sizing,
            crate::BoxSizing::Border
        );
        assert_eq!(
            layout("<!doctype html><input>", "input").box_sizing,
            crate::BoxSizing::Content
        );
    }

    /// `:lang()` reaches the page's own rules: the language is the nearest one
    /// declared, a range takes in the tags below it, and a page that says its
    /// language in a `<meta>` alone has it too.
    #[test]
    fn lang_selectors_match_the_language_declared_above() {
        let green = |rule: &str, html: &str| {
            let page =
                format!("<style>.l1 {{ color: red }} {rule} {{ color: #008000 }}</style>{html}");
            colour(&computed(&page, ".l1")) == (0, 128, 0)
        };
        assert!(green(":lang(en) .l1", "<html lang=en-FI><p class=l1>x"));
        assert!(!green(":lang(en) .l1", "<html lang=de><p class=l1>x"));
        assert!(!green(
            ".l1:lang(en)",
            "<html lang=en-FI><div lang=''><p class=l1>x"
        ));
        assert!(green(
            ".l1:lang(en)",
            "<meta http-equiv=Content-Language content=en-GB><p class=l1>x"
        ));
    }

    /// A mouse points precisely and hovers, so the queries that ask whether the
    /// pointer can are answered yes.
    #[test]
    fn the_pointer_is_a_mouse() {
        let viewport = Viewport::default();
        for yes in [
            "(hover: hover)",
            "(any-hover: hover)",
            "(pointer: fine)",
            "(any-pointer: fine)",
        ] {
            assert!(media_condition_matches(yes, viewport), "{yes}");
        }
        for no in [
            "(hover: none)",
            "(any-hover: none)",
            "(pointer: coarse)",
            "(pointer: none)",
        ] {
            assert!(!media_condition_matches(no, viewport), "{no}");
        }
    }

    /// A `@font-face` rule names a family and the addresses it may be fetched
    /// from, including the ones inside a media query — and a `local()` source is
    /// an installed family rather than an address.
    #[test]
    fn font_face_rules_are_collected() {
        let document = otlyra_html::parse(
            br#"<style>
              @font-face { font-family: "Brought"; src: url(a.woff2) format("woff2"), url(a.ttf); }
              @media (min-width: 1px) {
                @font-face { font-family: Queried; src: local("Helvetica"), url("q.otf"); }
              }
              @font-face { font-family: Nowhere; src: local("Helvetica"); }
            </style>"#,
            Some("utf-8"),
        )
        .document;

        let styler = Styler::new(&document, Viewport::default(), &at("https://x.test/p/"));
        let faces = styler.font_faces();

        assert_eq!(
            faces.len(),
            2,
            "a rule with no address names no font: {faces:?}"
        );
        let brought = faces
            .iter()
            .find(|face| face.family == "Brought")
            .expect("the first rule");
        assert_eq!(
            addresses(brought),
            ["https://x.test/p/a.woff2", "https://x.test/p/a.ttf"],
            "in the order written, against the document's base"
        );

        let queried = faces
            .iter()
            .find(|face| face.family == "Queried")
            .expect("the rule inside the query");
        assert_eq!(addresses(queried), ["https://x.test/p/q.otf"]);
    }

    /// A rule in a fetched sheet names its fonts against that sheet's own
    /// address, not the page's.
    #[test]
    fn a_font_face_resolves_against_its_own_sheet() {
        let document =
            otlyra_html::parse(b"<link rel=stylesheet href=of/its/own.css>", Some("utf-8"))
                .document;
        let sources = linked(
            &document,
            &[(
                "https://x.test/of/its/own.css",
                "@font-face { font-family: Far; src: url(../fonts/far.woff2) }",
            )],
        );

        let styler = Styler::new(&document, Viewport::default(), &sources);
        let face = styler.font_faces().first().expect("the rule");
        assert_eq!(face.family, "Far");
        assert_eq!(addresses(face), ["https://x.test/of/fonts/far.woff2"]);
    }

    /// A resize that no rule reads changes nothing, and the caller is told so:
    /// this is what turns a window drag into a relayout rather than a re-cascade of
    /// the whole document.
    #[test]
    fn a_resize_only_restyles_when_a_rule_reads_the_viewport() {
        let plain =
            otlyra_html::parse(b"<style>p { color: red }</style><p>x", Some("utf-8")).document;
        let mut styler = Styler::new(&plain, Viewport::default(), &StyleSources::default());
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
        let mut styler = Styler::new(&queried, Viewport::default(), &StyleSources::default());
        styler.style(&queried);
        assert!(
            styler.resize(Viewport {
                width: 400.0,
                ..Viewport::default()
            }),
            "the query stopped matching"
        );
    }

    /// The one preference of the reader's that half the web reads.
    #[test]
    fn prefers_color_scheme_answers_the_environment() {
        const PAGE: &str = "<style>\
             p { color: rgb(0, 0, 0) }\
             @media (prefers-color-scheme: dark) { p { color: rgb(255, 255, 255) } }\
             </style><p>x";

        let document = otlyra_html::parse(PAGE.as_bytes(), Some("utf-8")).document;
        let node = crate::stylo_dom::select(&document, "p")
            .expect("the selector should parse")
            .into_iter()
            .next()
            .expect("a paragraph");

        let light = style_document(&document, Viewport::default());
        assert_eq!(colour(light.style_of(node).expect("a style")), (0, 0, 0));

        let dark = style_document(
            &document,
            Viewport {
                color_scheme: ColorScheme::Dark,
                ..Viewport::default()
            },
        );
        assert_eq!(
            colour(dark.style_of(node).expect("a style")),
            (255, 255, 255)
        );
    }

    /// The scheme goes to the sheets the same way a new width does, so a page
    /// that never asks keeps every style it computed.
    #[test]
    fn a_new_scheme_restyles_only_a_page_that_asked() {
        let plain =
            otlyra_html::parse(b"<style>p { color: red }</style><p>x", Some("utf-8")).document;
        let mut styler = Styler::new(&plain, Viewport::default(), &StyleSources::default());
        styler.style(&plain);
        assert!(
            !styler.resize(Viewport {
                color_scheme: ColorScheme::Dark,
                ..Viewport::default()
            }),
            "nothing in this document reads the scheme"
        );

        let queried = otlyra_html::parse(
            b"<style>@media (prefers-color-scheme: dark) { p { color: red } }</style><p>x",
            Some("utf-8"),
        )
        .document;
        let mut styler = Styler::new(&queried, Viewport::default(), &StyleSources::default());
        styler.style(&queried);
        assert!(
            styler.resize(Viewport {
                color_scheme: ColorScheme::Dark,
                ..Viewport::default()
            }),
            "the query started matching"
        );
    }

    /// A viewport unit is read while cascading rather than while matching, so the
    /// engine only knows it was used once it has been.
    #[test]
    fn a_viewport_unit_makes_every_resize_a_restyle() {
        let document =
            otlyra_html::parse(b"<style>p { width: 50vw }</style><p>x", Some("utf-8")).document;
        let mut styler = Styler::new(&document, Viewport::default(), &StyleSources::default());
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
        let styled = style_document_with(&document, Viewport::default(), &StyleSources::default());
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

    /// Monospace has a base of its own, and it is the `font-size` *keyword* that
    /// carries it: an element inherits the keyword and the ratio applied to it, so
    /// the same `<code>` is thirteen pixels in prose and scaled with its heading
    /// inside one — and none of it survives a size written as a length.
    #[test]
    fn monospace_starts_from_a_smaller_size_than_prose() {
        let size = |html: &str, selector: &str| {
            computed(html, selector).clone_font_size().used_size().px()
        };

        assert_eq!(size("<body><p>text", "p"), 16.0);
        assert_eq!(size("<body><p><code>x", "code"), 13.0);
        assert_eq!(size("<body><pre>x", "pre"), 13.0);
        // A heading is 1.5em, and the ratio travels with the keyword.
        assert_eq!(size("<body><h2><code>x", "code"), 19.5);
        // A length breaks the chain, as it does in every browser.
        assert_eq!(
            size(
                "<style>div { font-size: 32px }</style><body><div><code>x",
                "code"
            ),
            32.0
        );
    }

    /// `rem` is the root element's font size, not sixteen pixels. A page that
    /// sets its own — which is most of the modern web, usually to make every
    /// length in the design one round number — has every `rem` on it wrong
    /// otherwise, and wrong by the same fraction everywhere, which looks less
    /// like a unit bug than like a browser that cannot lay out.
    #[test]
    fn rem_is_the_root_elements_font_size_and_not_the_initial_one() {
        let size = |html: &str, selector: &str| {
            computed(html, selector).clone_font_size().used_size().px()
        };

        // Without a root size of its own, `rem` is the initial sixteen.
        assert_eq!(
            size("<style>p { font-size: 2rem }</style><body><p>x", "p"),
            32.0
        );
        // With one, it is that.
        assert_eq!(
            size(
                "<style>html { font-size: 20px } p { font-size: 2rem }</style><body><p>x",
                "p"
            ),
            40.0
        );
        // Including when the root names its own size relatively, which is how a
        // page usually does it: `1.25em` of the initial sixteen is twenty.
        assert_eq!(
            size(
                "<style>html { font-size: 1.25em } p { font-size: 2rem }</style><body><p>x",
                "p"
            ),
            40.0
        );
    }

    /// And it reaches lengths that are not font sizes, which is most of what a
    /// page writes in `rem`.
    #[test]
    fn rem_reaches_every_length_and_not_only_the_font() {
        let styled = computed(
            "<style>html { font-size: 20px } div { width: 10rem }</style><body><div>x",
            "div",
        );
        let width = format!("{:?}", styled.clone_width());
        assert!(
            width.contains("200"),
            "ten rem of a twenty-pixel root: {width}"
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
    /// The first background picture's address in a computed style.
    fn background(style: &ComputedValues) -> Option<String> {
        crate::computed::to_layout_style(style)
            .backgrounds
            .first()
            .and_then(|layer| layer.image.as_deref().map(str::to_owned))
    }

    /// A linked sheet's relative addresses resolve against the sheet (CSS Values
    /// 4 §4.5.1), wherever the page that links it is.
    #[test]
    fn a_sheet_resolves_its_urls_against_its_own_address() {
        let style = styled(
            "<link rel=stylesheet href=css/a.css><div>x</div>",
            |document| {
                linked(
                    document,
                    &[(
                        "https://x.test/css/a.css",
                        "div { background: url(../i/b.png) }",
                    )],
                )
            },
            "div",
        );
        assert_eq!(
            background(&style).as_deref(),
            Some("https://x.test/i/b.png")
        );
    }

    /// A `<style>` and a `style` attribute have no address of their own, and
    /// resolve theirs against the document's base.
    #[test]
    fn inline_style_resolves_against_the_documents_base() {
        let html = "<style>p { background-image: url(sheet.png) }</style><p>x</p>\
                    <div style=\"background-image: url(attribute.png)\">y</div>";
        let cdn = |_: &Document| at("https://cdn.test/assets/");
        assert_eq!(
            background(&styled(html, cdn, "p")).as_deref(),
            Some("https://cdn.test/assets/sheet.png")
        );
        assert_eq!(
            background(&styled(html, cdn, "div")).as_deref(),
            Some("https://cdn.test/assets/attribute.png")
        );
    }

    /// A link's `media` holds for the whole sheet however the sheet is written.
    /// Wrapping the text in an `@media` block let a stray `}` close the block
    /// early and the rules after it apply everywhere.
    #[test]
    fn a_stray_brace_cannot_escape_a_links_media() {
        let print = computed_with_links(
            "<link rel=stylesheet href=a.css media=print><body><p>x",
            &["a { color: blue } } p { color: rgb(0, 128, 0) }"],
            "p",
        );
        assert_ne!(colour(&print), (0, 128, 0));
    }

    /// The sources for a page whose one link is `https://x.test/css/main.css`,
    /// with the sheets its imports fetched, each at the address it was asked
    /// for.
    fn with_imports(document: &Document, main: &str, imports: &[(&str, &str)]) -> StyleSources {
        StyleSources {
            imports: imports
                .iter()
                .map(|(url, text)| (Url::parse(url).expect("parses"), fetched(url, text)))
                .collect(),
            ..linked(document, &[("https://x.test/css/main.css", main)])
        }
    }

    const IMPORTING: &str = "<link rel=stylesheet href=css/main.css><body><p>x</p><div>y</div>";

    /// An imported sheet cascades where the `@import` stands: before the rest of
    /// the sheet that imports it, which therefore wins a tie.
    #[test]
    fn an_imported_sheet_cascades_where_it_is_imported() {
        let sources = |document: &Document| {
            with_imports(
                document,
                "@import url(base.css); div { color: rgb(0, 0, 255) }",
                &[(
                    "https://x.test/css/base.css",
                    "p { color: rgb(0, 128, 0) } div { color: rgb(255, 0, 0) }",
                )],
            )
        };
        assert_eq!(colour(&styled(IMPORTING, sources, "p")), (0, 128, 0));
        assert_eq!(colour(&styled(IMPORTING, sources, "div")), (0, 0, 255));
    }

    /// An import's media list is the imported sheet's, and the engine applies it.
    #[test]
    fn an_import_for_print_does_not_style_the_screen() {
        let importing = |condition: &'static str| {
            move |document: &Document| {
                with_imports(
                    document,
                    condition,
                    &[("https://x.test/css/p.css", "p { color: rgb(0, 128, 0) }")],
                )
            }
        };
        let print = styled(IMPORTING, importing("@import url(p.css) print;"), "p");
        assert_ne!(colour(&print), (0, 128, 0));
        let screen = styled(IMPORTING, importing("@import url(p.css) screen;"), "p");
        assert_eq!(colour(&screen), (0, 128, 0));
    }

    /// An import inside an imported sheet is resolved against that sheet, and so
    /// is every address in it — its fonts included.
    #[test]
    fn a_nested_import_resolves_against_the_sheet_it_is_in() {
        let html = IMPORTING;
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let sources = with_imports(
            &document,
            "@import 'sub/a.css';",
            &[
                ("https://x.test/css/sub/a.css", "@import '../../b.css';"),
                (
                    "https://x.test/b.css",
                    "@font-face { font-family: Deep; src: url(f/deep.woff2) } \
                     p { background-image: url(i.png) }",
                ),
            ],
        );
        let mut styler = Styler::new(&document, Viewport::default(), &sources);
        let styled = styler.style(&document);
        let p = crate::stylo_dom::select(&document, "p").expect("a selector")[0];
        assert_eq!(
            background(styled.style_of(p).expect("a styled paragraph")).as_deref(),
            Some("https://x.test/i.png")
        );
        let faces = styler.font_faces();
        assert_eq!(faces.len(), 1, "{faces:?}");
        assert_eq!(addresses(&faces[0]), ["https://x.test/f/deep.woff2"]);
    }

    /// Two sheets that import each other are each parsed once, and the page is
    /// styled; a sheet that imports itself imports nothing.
    #[test]
    fn an_import_cycle_ends() {
        let sources = |document: &Document| {
            with_imports(
                document,
                "@import 'main.css'; @import 'a.css';",
                &[
                    ("https://x.test/css/main.css", "p { color: rgb(255, 0, 0) }"),
                    (
                        "https://x.test/css/a.css",
                        "@import 'b.css'; p { color: rgb(0, 128, 0) }",
                    ),
                    (
                        "https://x.test/css/b.css",
                        "@import 'a.css'; div { color: rgb(0, 0, 255) }",
                    ),
                ],
            )
        };
        assert_eq!(colour(&styled(IMPORTING, sources, "p")), (0, 128, 0));
        assert_eq!(colour(&styled(IMPORTING, sources, "div")), (0, 0, 255));
    }

    /// The addresses a sheet imports are resolved against it, in order; one whose
    /// `supports()` is false is not asked for, and an `@import` after a rule is
    /// not an import at all (CSS Cascade 5 §2).
    #[test]
    fn imports_are_discovered_against_their_sheet() {
        let sheet = Url::parse("https://x.test/css/main.css").expect("parses");
        let found = imports_in(
            "@charset 'utf-8'; @import 'a.css'; @import url(../b.css) print; \
             @import 'c.css' supports(nonsense: 1); @import 'd.css' supports(display: grid); \
             p { color: red } @import 'late.css';",
            &sheet,
        );
        let found: Vec<&str> = found.iter().map(Url::as_str).collect();
        assert_eq!(
            found,
            [
                "https://x.test/css/a.css",
                "https://x.test/b.css",
                "https://x.test/css/d.css",
            ]
        );
    }

    /// An import whose `supports()` is false is refused where the sheet is
    /// parsed, not only left unfetched: the engine does not ask the condition
    /// itself, and a sheet fetched for another import of the same address must
    /// not apply through this one.
    #[test]
    fn an_import_whose_supports_is_false_is_refused() {
        let sources = |document: &Document| {
            with_imports(
                document,
                "@import url(c.css) supports(not (display: block)); \
                 @import url(c.css) layer(low) supports(display: block); \
                 @layer high { p { color: rgb(0, 0, 255) } }",
                &[(
                    "https://x.test/css/c.css",
                    "p, div { color: rgb(0, 128, 0) }",
                )],
            )
        };
        // Unlayered, the false import would beat every layer and turn `p` green.
        assert_eq!(colour(&styled(IMPORTING, sources, "p")), (0, 0, 255));
        assert_eq!(colour(&styled(IMPORTING, sources, "div")), (0, 128, 0));
    }

    /// A redirected sheet is known by where it ended up as well as by what was
    /// asked for, so importing itself at its new address is the cycle it is and
    /// the sheet is parsed once.
    #[test]
    fn an_import_cycle_through_a_redirect_ends() {
        const MOVED: &str = "@import 'a.css'; \
             @font-face { font-family: Moved; src: url(m.woff2) } \
             p { color: rgb(0, 128, 0) }";
        let document = otlyra_html::parse(IMPORTING.as_bytes(), Some("utf-8")).document;
        let mut sources = with_imports(
            &document,
            "@import 'a.css';",
            &[("https://x.test/moved/a.css", MOVED)],
        );
        sources.imports.insert(
            Url::parse("https://x.test/css/a.css").expect("parses"),
            fetched("https://x.test/moved/a.css", MOVED),
        );

        let mut styler = Styler::new(&document, Viewport::default(), &sources);
        let styled = styler.style(&document);
        let p = crate::stylo_dom::select(&document, "p").expect("a selector")[0];
        assert_eq!(
            colour(styled.style_of(p).expect("a styled paragraph")),
            (0, 128, 0)
        );
        let faces = styler.font_faces();
        assert_eq!(faces.len(), 1, "parsed once: {faces:?}");
        assert_eq!(addresses(&faces[0]), ["https://x.test/moved/m.woff2"]);
    }

    /// The inspector names a rule from an imported sheet by the selector it was
    /// written with, as it names one in the sheet that imports it.
    #[test]
    fn an_imported_rule_is_named_by_its_selector() {
        let document = otlyra_html::parse(IMPORTING.as_bytes(), Some("utf-8")).document;
        let sources = with_imports(
            &document,
            "@import 'base.css';",
            &[(
                "https://x.test/css/base.css",
                "body > p { color: rgb(0, 128, 0) }",
            )],
        );
        let mut styler = Styler::new(&document, Viewport::default(), &sources);
        let styled = styler.style(&document);
        let p = crate::stylo_dom::select(&document, "p").expect("a selector")[0];
        let rules = styler.rules_for(styled.style_of(p).expect("a styled paragraph"));
        assert!(
            rules
                .iter()
                .any(|rule| rule.selector == "body > p" && rule.origin == "page"),
            "{rules:?}"
        );
    }

    /// However often a page imports what it fetched, no more than
    /// [`IMPORTED_SHEET_LIMIT`] imports are parsed.
    #[test]
    fn imports_past_the_limit_are_refused() {
        let document = otlyra_html::parse(IMPORTING.as_bytes(), Some("utf-8")).document;
        let main = "@import 'leaf.css';".repeat(IMPORTED_SHEET_LIMIT * 2);
        let sources = with_imports(
            &document,
            &main,
            &[(
                "https://x.test/css/leaf.css",
                "@font-face { font-family: Leaf; src: url(leaf.woff2) }",
            )],
        );
        let styler = Styler::new(&document, Viewport::default(), &sources);
        assert_eq!(styler.font_faces().len(), IMPORTED_SHEET_LIMIT);
    }
}

#[cfg(test)]
mod media_attribute_tests {
    use super::*;

    fn paragraph_is_red(html: &str, width: f32) -> bool {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let styles = style_document(
            &parsed.document,
            Viewport {
                width,
                height: 600.0,
                scale: 1.0,
                text_scale: 1.0,
                color_scheme: Default::default(),
            },
        );
        let node = crate::stylo_dom::select(&parsed.document, "p")
            .expect("the selector parses")
            .into_iter()
            .next()
            .expect("a paragraph");
        let style = styles.style_of(node).expect("a styled paragraph");
        let colour = style
            .clone_color()
            .to_color_space(style::color::ColorSpace::Srgb);
        let channel = |value: f32| (value * 255.0).round() as u8;
        (
            channel(colour.components.0),
            channel(colour.components.1),
            channel(colour.components.2),
        ) == (255, 0, 0)
    }

    /// `media` on a `<style>` block is what it says on a `<link>`: the block is
    /// for that medium and for no other. Without this a sheet an author wrote
    /// for the printer styles the screen.
    #[test]
    fn a_style_block_written_for_print_does_not_style_the_screen() {
        assert!(
            !paragraph_is_red(
                "<style media=print>p { color: #ff0000 }</style><body><p>x",
                800.0
            ),
            "the print block reached the screen"
        );
        // The same block with no attribute does apply, so the case above is
        // about the attribute rather than about the block being dropped.
        assert!(paragraph_is_red(
            "<style>p { color: #ff0000 }</style><body><p>x",
            800.0
        ));
        // And a condition that does match is honoured, in both directions.
        assert!(paragraph_is_red(
            "<style media='(min-width: 500px)'>p { color: #ff0000 }</style><body><p>x",
            800.0
        ));
        assert!(!paragraph_is_red(
            "<style media='(min-width: 500px)'>p { color: #ff0000 }</style><body><p>x",
            400.0
        ));
    }

    /// And a link carries what it says it is for, so the browser can decide
    /// whether to wait for it.
    #[test]
    fn a_link_reports_the_medium_it_names() {
        let parsed = otlyra_html::parse(
            b"<link rel=stylesheet href=a.css><link rel=stylesheet media=print href=b.css>",
            Some("utf-8"),
        );
        let links = stylesheet_links(&parsed.document);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].media, "");
        assert_eq!(links[1].media, "print");
    }
}
