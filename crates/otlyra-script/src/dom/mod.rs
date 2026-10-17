//! The document, as script sees it.
//!
//! ## The loan
//!
//! The engine's native functions are either bare `fn` pointers or closures the
//! engine requires to be `Send + Sync`, so neither can carry a borrow of our
//! arena: a `&mut Document` cannot be captured, and the `Document` itself is
//! full of `StrTendril`s and is not `Send`. The way through is the one Otter's
//! own Node layer uses for its file table — a thread-local the natives read
//! from — with one addition that makes it safe rather than merely convenient:
//! the document is *moved* into the thread-local for the duration of a script
//! turn and moved back out when the turn ends.
//!
//! That is what [`loan`] does. Outside a turn the slot is empty and every
//! binding fails cleanly; inside one, exactly one document is reachable and the
//! borrow checker is not being lied to. A panic mid-turn still returns the
//! document, because the restoring is a `Drop`.
//!
//! The isolate is pinned to one thread and a page's turns all run on it, so
//! "thread-local" and "this page's document" are the same statement.
//!
//! ## Whose document
//!
//! A wrapper carries the id of the document it names, and every access checks
//! it against the one that is lent. That is what makes two open pages sharing
//! one thread safe rather than merely unlikely: a wrapper from another page
//! finds nothing, however the two isolates come to be scheduled. It is also
//! what the plan asks for (§8.3) and what Chromium does by a different
//! mechanism — the wrapper carries the identity of what it points at.
//!
//! The remaining thread-local is the *loan itself*, and it goes when the engine
//! grows an isolate-local embedder slot (Otter's equivalent of
//! `v8::Isolate::SetData`, which is what Chromium reaches this through). Until
//! then the isolate is thread-pinned and a turn is synchronous, so the slot is
//! empty outside a turn and holds exactly one document inside one.
//!
//! ## What script may change
//!
//! Every mutation sets a flag, which the caller reads with [`take_dirty`] after
//! the turn. Style, layout and paint then re-run. Nothing here decides how much
//! of that is needed — a binding that guessed at damage levels would be a
//! second, quieter invalidation system beside the real one.

mod canvas;
mod character_data;
mod document;
mod element;
mod identity;
#[cfg(test)]
mod interface_tree;
mod interfaces;
mod markup;
mod node;
mod tree;

use std::cell::RefCell;

use otlyra_dom::{Document, DocumentId};
use otter_runtime::marshal::JsError;

pub use identity::{JsNode, forget_all as forget_wrappers, wrapper_count};
pub use interfaces::{element_interface, is_valid_custom_element_name};
pub use node::{
    CharacterDataRef, CommentRef, DocumentFragmentRef, DocumentRef, DocumentTypeRef, ElementRef,
    NodeRef, TextRef,
};
/// Where script asked to go. Declared beside the parser's script point, because
/// that is the seam the browser reads it across.
pub use otlyra_html::Navigation;

thread_local! {
    /// The document this thread's isolate is currently allowed to touch.
    static LOANED: RefCell<Option<Document>> = const { RefCell::new(None) };
    /// The page state that goes with it.
    static STATE: RefCell<Option<PageState>> = const { RefCell::new(None) };
}

/// What one page's script turn reads and writes, besides the document.
///
/// Owned by the page, lent to the isolate for the length of a turn exactly as
/// the document is. It is not a thread-local because a thread has several pages
/// on it: a counter of owed animation frames kept per thread would put every
/// tab into its isolate because one of them animates, and an address kept per
/// thread would make `location.href` in an old tab report the address of
/// whichever tab loaded last.
///
/// A binding reaches it through [`with_state`] / [`with_state_mut`], which is
/// how it reaches the document too.
#[derive(Debug, Default)]
pub struct PageState {
    /// Whether script has changed the document since the browser last asked.
    dirty: bool,
    /// Whether an animation frame has been asked for and not yet given.
    frames_owed: bool,
    /// Whether the parser has finished, which is what `readyState` reports.
    ready: bool,
    /// Whether this document's tab or native window is hidden.
    hidden: bool,
    /// CSS viewport and device scale supplied by the embedder.
    viewport: WindowViewport,
    /// Whether the reader asks pages for the dark palette.
    dark: bool,
    // None in the cheap default placeholder used while a state is loaned.
    clock: Option<crate::clock::PageClock>,
    /// Where the page's script thinks it is. `location` is built from it.
    document_url: String,
    /// Where script asked to go, if it asked.
    navigation: Option<Navigation>,
    /// The `<script>` element whose code is running, which is what
    /// `document.currentScript` answers. Bundlers find their own address
    /// through it, and load the rest of themselves relative to that.
    current_script: Option<otlyra_dom::NodeId>,
    /// The element a user-input event the embedder is dispatching is aimed
    /// at, for the turn that dispatches it.
    input_target: Option<otlyra_dom::NodeId>,
    /// Whether the page listens for pointer movement, so the browser sends it
    /// rather than entering the isolate for every move of the mouse.
    wants_pointer_moves: bool,
    /// The focus script asked for in this turn: an element, or none for a
    /// blur. The browser applies it, and fires the focus events, after it.
    focus_request: Option<Option<otlyra_dom::NodeId>>,
    /// Where script asked the window to scroll to in this turn.
    scroll_request: Option<(f64, f64)>,
    /// Whether the page listens for scroll events.
    wants_scroll_events: bool,
    /// Whether an intersection or resize observer is watching.
    wants_geometry: bool,
    /// Whether the page listens for wheel events.
    wants_wheel_events: bool,
    /// Pictures asked for by `<img>` elements outside the document — a
    /// `new Image()` preloading — which nothing else would fetch.
    image_requests: Vec<otlyra_dom::NodeId>,
    /// Each `<canvas>` element's 2D bitmap, made when script first draws.
    canvases: std::collections::HashMap<otlyra_dom::NodeId, otlyra_gfx::canvas2d::Canvas2D>,
    /// The canvases drawn into since the browser last took their pixels.
    dirty_canvases: Vec<otlyra_dom::NodeId>,
    /// The page's dedicated workers.
    pub(crate) workers: crate::worker::Workers,
    /// The page's WebSockets and event streams.
    pub(crate) sockets: crate::sockets::Sockets,
    /// Where `document.cookie` reads and writes. `None` is a document with no
    /// cookies at all, which reads as empty and keeps nothing.
    cookies: Option<Cookies>,
    /// `localStorage` and `sessionStorage` as the browser keeps them, shared
    /// with the origin's other documents. `None` keeps them in
    /// [`PageState::own_storage`].
    storage: Option<[std::sync::Arc<crate::storage::StorageArea>; 2]>,
    /// Storage that is this document's alone: for a document the browser
    /// gave none, and for one with no origin to share it under.
    own_storage: [std::sync::Arc<crate::storage::StorageArea>; 2],
    /// Whether the page has defined a custom element, so that elements the
    /// parser made since script last ran are looked at before it runs again.
    custom_elements: bool,
    /// Where `history.pushState` or `replaceState` moved the document, for
    /// the browser to show; the document itself was not fetched again.
    url_changes: Vec<otlyra_html::UrlChange>,
    /// Requests `fetch` and `XMLHttpRequest` made that the browser has not
    /// taken yet, and the number the next one gets.
    page_requests: Vec<otlyra_html::PageRequest>,
    next_request: u64,
    /// Answers the browser delivered that script has not taken yet.
    page_responses: std::collections::HashMap<u64, Result<otlyra_html::PageResponse, String>>,
}

/// The browser's cookie jar, as `document.cookie` reaches it.
///
/// A trait rather than the jar itself so that this crate keeps no network: the
/// browser owns the jar, and decides which document may see which cookies.
pub trait DocumentCookies: Send + Sync {
    /// The cookies a document at `url` may read, as `document.cookie` spells
    /// them: `name=value` pairs joined by `"; "`.
    fn read(&self, url: &str) -> String;
    /// One cookie written to `document.cookie` by a document at `url`, as a
    /// `Set-Cookie` line. One that may not be kept is dropped.
    fn write(&self, url: &str, line: &str);
}

/// A handle on [`DocumentCookies`], which the state can carry and print.
#[derive(Clone)]
pub(crate) struct Cookies(pub(crate) std::sync::Arc<dyn DocumentCookies>);

impl std::fmt::Debug for Cookies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cookies")
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct WindowViewport {
    pub(crate) width: f64,
    pub(crate) height: f64,
    pub(crate) scale: f64,
}

impl Default for WindowViewport {
    fn default() -> Self {
        Self {
            width: 0.0,
            height: 0.0,
            scale: 1.0,
        }
    }
}

impl WindowViewport {
    pub(crate) fn new(width: f64, height: f64, scale: f64) -> Self {
        Self {
            width: if width.is_finite() {
                width.max(0.0)
            } else {
                0.0
            },
            height: if height.is_finite() {
                height.max(0.0)
            } else {
                0.0
            },
            scale: if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            },
        }
    }
}

impl PageState {
    /// The state of a page at `document_url`, before any of its script has run.
    #[must_use]
    pub fn new(document_url: String) -> Self {
        Self {
            document_url,
            clock: Some(crate::clock::PageClock::new()),
            ..Self::default()
        }
    }

    /// Give `document.cookie` somewhere to read and write.
    pub fn set_cookies(&mut self, cookies: std::sync::Arc<dyn DocumentCookies>) {
        self.cookies = Some(Cookies(cookies));
    }

    /// Give `localStorage` and `sessionStorage` the browser's areas, which
    /// the origin's other documents share.
    pub fn set_storage(
        &mut self,
        local: std::sync::Arc<crate::storage::StorageArea>,
        session: std::sync::Arc<crate::storage::StorageArea>,
    ) {
        self.storage = Some([local, session]);
    }

    /// Whether script changed the document, clearing the answer.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Whether an animation frame is owed.
    #[must_use]
    pub fn frames_owed(&self) -> bool {
        self.frames_owed
    }

    /// Forget the frames owed: the frame that would run them is being run now.
    pub fn clear_frames_owed(&mut self) {
        self.frames_owed = false;
    }

    /// Say whether the document has finished parsing.
    pub fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    /// Set visibility before parsing, or between script turns.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    /// Whether the document can receive rendering callbacks.
    #[must_use]
    pub fn hidden(&self) -> bool {
        self.hidden
    }

    pub(crate) fn viewport(&self) -> WindowViewport {
        self.viewport
    }

    /// Whether the reader asks for the dark palette.
    pub(crate) fn dark(&self) -> bool {
        self.dark
    }

    /// Say which palette the reader asks for.
    pub(crate) fn set_dark(&mut self, dark: bool) {
        self.dark = dark;
    }

    pub(crate) fn set_viewport(&mut self, viewport: WindowViewport) {
        self.viewport = viewport;
    }

    pub(crate) fn clock(&self) -> Option<crate::clock::PageClock> {
        self.clock
    }
    pub(crate) fn set_clock(&mut self, clock: Option<crate::clock::PageClock>) {
        self.clock = clock;
    }

    /// The requests script made since the browser last asked.
    pub fn take_page_requests(&mut self) -> Vec<otlyra_html::PageRequest> {
        std::mem::take(&mut self.page_requests)
    }

    /// Keep an answer for script to take.
    pub fn stage_page_response(
        &mut self,
        id: u64,
        response: Result<otlyra_html::PageResponse, String>,
    ) {
        self.page_responses.insert(id, response);
    }

    /// Where the session history moved the document without a load, if it
    /// moved since the browser last asked.
    pub fn take_url_changes(&mut self) -> Vec<otlyra_html::UrlChange> {
        std::mem::take(&mut self.url_changes)
    }

    /// Where script asked to go, if it asked.
    pub fn take_navigation(&mut self) -> Option<Navigation> {
        self.navigation.take()
    }

    /// Say which `<script>` element is running, or that none is: a timer, an
    /// event listener and a frame callback run with none.
    pub fn set_current_script(&mut self, element: Option<otlyra_dom::NodeId>) {
        self.current_script = element;
    }

    /// Take where script asked the window to scroll to, if it asked.
    pub fn take_scroll_request(&mut self) -> Option<(f64, f64)> {
        self.scroll_request.take()
    }

    /// Take the pictures detached elements asked for.
    pub fn take_image_requests(&mut self) -> Vec<otlyra_dom::NodeId> {
        std::mem::take(&mut self.image_requests)
    }

    /// Take what the page's workers sent: their requests become page
    /// requests, and whether messages or errors wait for the glue.
    pub fn drain_workers(&mut self) -> bool {
        let Self {
            workers,
            page_requests,
            next_request,
            ..
        } = self;
        workers.drain(|url, script| {
            *next_request += 1;
            page_requests.push(otlyra_html::PageRequest {
                id: *next_request,
                method: "GET".to_owned(),
                url,
                headers: Vec::new(),
                body: None,
                script,
            });
            *next_request
        }) || workers.has_events()
    }

    /// Take a copy of each canvas bitmap drawn into since this was last
    /// asked: what the page shows for those elements now.
    pub fn take_canvas_frames(&mut self) -> Vec<(otlyra_dom::NodeId, otlyra_dom::Bitmap)> {
        let dirty = std::mem::take(&mut self.dirty_canvases);
        dirty
            .into_iter()
            .filter_map(|node| {
                let canvas = self.canvases.get_mut(&node)?;
                let (width, height) = canvas.size();
                let pixels: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync> =
                    std::sync::Arc::new(canvas.pixels());
                Some((
                    node,
                    otlyra_dom::Bitmap {
                        width: width.max(1),
                        height: height.max(1),
                        premultiplied: true,
                        pixels,
                    },
                ))
            })
            .collect()
    }

    /// Whether the page listens for wheel events.
    #[must_use]
    pub fn wants_wheel_events(&self) -> bool {
        self.wants_wheel_events
    }

    /// Whether an intersection or resize observer is watching.
    #[must_use]
    pub fn wants_geometry(&self) -> bool {
        self.wants_geometry
    }

    /// Whether the page listens for scroll events.
    #[must_use]
    pub fn wants_scroll_events(&self) -> bool {
        self.wants_scroll_events
    }

    /// Take the focus script asked for, if it asked.
    pub fn take_focus_request(&mut self) -> Option<Option<otlyra_dom::NodeId>> {
        self.focus_request.take()
    }

    /// Whether the page listens for pointer movement.
    #[must_use]
    pub fn wants_pointer_moves(&self) -> bool {
        self.wants_pointer_moves
    }

    /// Aim the user-input event about to be dispatched, or clear the aim.
    pub fn set_input_target(&mut self, element: Option<otlyra_dom::NodeId>) {
        self.input_target = element;
    }
}

/// Read the lent page state. A binding running outside a turn — our bug, not a
/// page's — sees a default one rather than a panic.
pub(crate) fn with_state<R>(read: impl FnOnce(&PageState) -> R) -> R {
    STATE.with(|slot| match slot.borrow().as_ref() {
        Some(state) => read(state),
        None => read(&PageState::default()),
    })
}

/// Change the lent page state, if there is one.
/// Change the page's state and answer something from it; `None` while no
/// page's state is loaned.
pub(crate) fn with_state_value<R>(change: impl FnOnce(&mut PageState) -> R) -> Option<R> {
    STATE.with(|slot| slot.borrow_mut().as_mut().map(change))
}

pub(crate) fn with_state_mut(change: impl FnOnce(&mut PageState)) {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            change(state);
        }
    });
}

pub(crate) fn document_url() -> String {
    with_state(|state| state.document_url.clone())
}

/// `history.pushState` and its kin moved the document to `url`, which the
/// caller already checked is same-origin.
pub(crate) fn set_document_url(url: String, kind: otlyra_html::HistoryMove) {
    with_state_mut(|state| {
        state.document_url.clone_from(&url);
        state.url_changes.push(otlyra_html::UrlChange { url, kind });
    });
}

/// Script asked the browser to send a request; the number its answer will
/// carry.
pub(crate) fn request_page_fetch(
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    script: bool,
) -> u64 {
    let mut id = 0;
    with_state_mut(|state| {
        state.next_request += 1;
        id = state.next_request;
        state.page_requests.push(otlyra_html::PageRequest {
            id,
            method,
            url,
            headers,
            body,
            script,
        });
    });
    id
}

/// The answer to request `id`, if the browser delivered one script has not
/// taken.
pub(crate) fn take_page_response(id: u64) -> Option<Result<otlyra_html::PageResponse, String>> {
    let mut taken = None;
    with_state_mut(|state| taken = state.page_responses.remove(&id));
    taken
}

/// The page defined a custom element.
pub(crate) fn note_custom_elements() {
    with_state_mut(|state| state.custom_elements = true);
}

/// Whether the page lent to this isolate has defined a custom element.
pub(crate) fn custom_elements_defined() -> bool {
    with_state(|state| state.custom_elements)
}

/// Whether mutation records are waiting for their observers' callbacks.
pub(crate) fn mutation_records_pending() -> bool {
    LOANED.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(Document::has_mutation_records)
    })
}

/// How many nodes the lent document has ever made. The arena only grows, so
/// a count that has not moved is a document with no new elements in it.
pub(crate) fn node_count() -> usize {
    LOANED.with(|slot| slot.borrow().as_ref().map_or(0, Document::len))
}

/// What `document.cookie` reads now.
pub(crate) fn read_cookies() -> String {
    with_state(|state| {
        state
            .cookies
            .as_ref()
            .map(|cookies| cookies.0.read(&state.document_url))
            .unwrap_or_default()
    })
}

/// The area `localStorage` (0) or `sessionStorage` (1) keeps this document's
/// items in, and the origin they are kept under. Only an http(s) document
/// shares an origin's storage; any other keeps its own for as long as it is
/// open.
pub(crate) fn storage_area(which: u32) -> (std::sync::Arc<crate::storage::StorageArea>, String) {
    with_state(|state| {
        let which = usize::from(which != 0);
        let origin = url::Url::parse(&state.document_url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .map(|url| url.origin().ascii_serialization());
        match (&state.storage, origin) {
            (Some(shared), Some(origin)) => (std::sync::Arc::clone(&shared[which]), origin),
            _ => (
                std::sync::Arc::clone(&state.own_storage[which]),
                String::new(),
            ),
        }
    })
}

/// One write to `document.cookie`.
pub(crate) fn write_cookie(line: &str) {
    with_state(|state| {
        if let Some(cookies) = &state.cookies {
            cookies.0.write(&state.document_url, line);
        }
    });
}

pub(crate) fn request_navigation(navigation: Navigation) {
    // Last one wins. A script that sets `location.href` twice in a turn has
    // changed its mind, and a browser goes where it ended up.
    with_state_mut(|state| state.navigation = Some(navigation));
}

/// Script asked the window to scroll to `(x, y)`: recorded for the browser,
/// and answered at once to script reading the lent document.
pub(crate) fn request_scroll(x: f64, y: f64) {
    with_state_mut(|state| state.scroll_request = Some((x, y)));
    LOANED.with(|slot| {
        if let Some(document) = slot.borrow_mut().as_mut() {
            document.set_scroll_position((x, y));
        }
    });
}

/// A detached `<img>` was given a picture to show.
pub(crate) fn request_image(node: otlyra_dom::NodeId) {
    with_state_mut(|state| {
        if !state.image_requests.contains(&node) {
            state.image_requests.push(node);
        }
    });
}

/// The page registered a wheel listener.
pub(crate) fn note_wheel_events_wanted() {
    with_state_mut(|state| state.wants_wheel_events = true);
}

/// An intersection or resize observer started watching.
pub(crate) fn note_geometry_wanted() {
    with_state_mut(|state| state.wants_geometry = true);
}

/// The page registered a scroll listener.
pub(crate) fn note_scroll_events_wanted() {
    with_state_mut(|state| state.wants_scroll_events = true);
}

/// Script asked to move the focus to `node`, or to drop it.
pub(crate) fn request_focus(node: Option<otlyra_dom::NodeId>) {
    with_state_mut(|state| state.focus_request = Some(node));
}

/// The page registered a listener for pointer movement.
pub(crate) fn note_pointer_moves_wanted() {
    with_state_mut(|state| state.wants_pointer_moves = true);
}

/// The page asked for an animation frame.
pub(crate) fn note_frame_request() {
    with_state_mut(|state| state.frames_owed = true);
}

pub(crate) fn is_ready() -> bool {
    with_state(|state| state.ready)
}

pub(crate) fn is_hidden() -> bool {
    with_state(|state| state.hidden)
}

pub(crate) fn time_metric(which: u32) -> f64 {
    with_state(|state| {
        state.clock.map_or(f64::NAN, |clock| match which {
            0 => clock.now(),
            1 => clock.time_origin(),
            _ => f64::NAN,
        })
    })
}

pub(crate) fn window_metric(which: u32) -> f64 {
    let viewport = with_state(|state| state.viewport);
    match which {
        0 => viewport.width.floor(),
        1 => viewport.height.floor(),
        _ => viewport.scale,
    }
}

/// Whether `query` matches the window this page is shown in, answered the
/// way the cascade answers `@media`.
pub(crate) fn media_matches(query: &str) -> bool {
    let (viewport, dark) = with_state(|state| (state.viewport, state.dark));
    otlyra_css::cascade::media_condition_matches(
        query,
        otlyra_css::cascade::Viewport {
            width: viewport.width as f32,
            height: viewport.height as f32,
            scale: viewport.scale as f32,
            color_scheme: if dark {
                otlyra_css::cascade::ColorScheme::Dark
            } else {
                otlyra_css::cascade::ColorScheme::Light
            },
            ..otlyra_css::cascade::Viewport::default()
        },
    )
}

/// Say which `<script>` element script is running now, for one it inserted.
pub(crate) fn set_current_script(element: Option<otlyra_dom::NodeId>) {
    with_state_mut(|state| state.current_script = element);
}

pub(crate) fn current_script() -> Option<otlyra_dom::NodeId> {
    with_state(|state| state.current_script)
}

pub(crate) fn input_target() -> Option<otlyra_dom::NodeId> {
    with_state(|state| state.input_target)
}

/// Lend `document` and `state` to the isolate for the duration of `run`.
///
/// Both are moved in and moved back; a caller still holding `&mut Document`
/// cannot reach it while script can, which is the whole point. The state goes
/// with the document because it is the same loan: what a turn writes about the
/// page — that it changed the document, that it wants a frame, where it asked
/// to go — belongs to the page whose turn it is, and that page is the one whose
/// document is lent.
///
/// Loans do not nest. A second one while the first is live would take an empty
/// slot, and script would run against a document with no nodes in it — so it
/// panics instead, on our own bug rather than on a page's.
pub fn loan<R>(document: &mut Document, state: &mut PageState, run: impl FnOnce() -> R) -> R {
    let taken = std::mem::take(document);
    let taken_state = std::mem::take(state);
    LOANED.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "a document is already lent to this isolate");
        *slot = Some(taken);
    });
    STATE.with(|slot| *slot.borrow_mut() = Some(taken_state));

    // The restoring is a `Drop` so that a panicking script — or a native that
    // unwinds — still gives the document back rather than leaving the page
    // holding an empty one.
    struct Restore<'a> {
        document: &'a mut Document,
        state: &'a mut PageState,
    }

    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            if let Some(document) = LOANED.with(|slot| slot.borrow_mut().take()) {
                *self.document = document;
            }
            if let Some(state) = STATE.with(|slot| slot.borrow_mut().take()) {
                *self.state = state;
            }
        }
    }

    let _restore = Restore { document, state };
    run()
}

/// Read the lent document.
///
/// The error is what a binding throws when there is none: a page whose script
/// somehow ran outside a turn is a bug in us, and it reads as one in the
/// console rather than as a mysterious `undefined`.
pub(crate) fn with_document<R>(
    owner: DocumentId,
    read: impl FnOnce(&Document) -> R,
) -> Result<R, JsError> {
    LOANED.with(|slot| match slot.borrow().as_ref() {
        Some(document) if document.id() == owner => Ok(read(document)),
        _ => Err(detached()),
    })
}

/// Read and change the lent document, marking it dirty.
pub(crate) fn with_document_mut<R>(
    owner: DocumentId,
    change: impl FnOnce(&mut Document) -> R,
) -> Result<R, JsError> {
    LOANED.with(|slot| match slot.borrow_mut().as_mut() {
        Some(document) if document.id() == owner => {
            with_state_mut(|state| state.dirty = true);
            Ok(change(document))
        }
        _ => Err(detached()),
    })
}

/// Change the lent document's bookkeeping without marking it changed: what a
/// read that has to style the tree first does, which leaves the tree as it
/// was.
pub(crate) fn with_document_unmarked<R>(
    owner: DocumentId,
    change: impl FnOnce(&mut Document) -> R,
) -> Result<R, JsError> {
    LOANED.with(|slot| match slot.borrow_mut().as_mut() {
        Some(document) if document.id() == owner => Ok(change(document)),
        _ => Err(detached()),
    })
}

/// Which document is lent to this isolate right now, if any.
pub(crate) fn lent_document() -> Option<DocumentId> {
    LOANED.with(|slot| slot.borrow().as_ref().map(Document::id))
}

fn detached() -> JsError {
    JsError::Type(
        "this node belongs to a document that is not available to script right now".to_owned(),
    )
}

otter_macros::romp! {
    name = "dom",
    ident = DOM_EXTENSION,
    // Parents before children: a subclass finds its parent's prototype off the
    // global when it is installed.
    classes = [
        node::NodeRefIntrinsic,
        element::ElementRefIntrinsic,
        character_data::CharacterDataRefIntrinsic,
        character_data::TextRefIntrinsic,
        character_data::CommentRefIntrinsic,
        document::DocumentRefIntrinsic,
        document::DocumentFragmentRefIntrinsic,
        document::DocumentTypeRefIntrinsic,
    ],
    // Compiled by `build.rs`, which also owns the list of globals it defines.
    js = Some(include!(concat!(env!("OUT_DIR"), "/dom-bootstrap.rs"))),
}
