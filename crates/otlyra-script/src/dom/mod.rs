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

mod identity;
mod node;

use std::cell::RefCell;

use otlyra_dom::{Document, DocumentId};
use otter_runtime::marshal::JsError;

pub use identity::{Wrapped, forget_all as forget_wrappers, wrapper_count};
pub use node::{DOM_EXTENSION, DocumentRef, ElementRef, NodeRef, TextRef};
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
    /// Where the page's script thinks it is. `location` is built from it.
    document_url: String,
    /// Where script asked to go, if it asked.
    navigation: Option<Navigation>,
}

impl PageState {
    /// The state of a page at `document_url`, before any of its script has run.
    #[must_use]
    pub fn new(document_url: String) -> Self {
        Self {
            document_url,
            ..Self::default()
        }
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

    /// Where script asked to go, if it asked.
    pub fn take_navigation(&mut self) -> Option<Navigation> {
        self.navigation.take()
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

pub(crate) fn request_navigation(navigation: Navigation) {
    // Last one wins. A script that sets `location.href` twice in a turn has
    // changed its mind, and a browser goes where it ended up.
    with_state_mut(|state| state.navigation = Some(navigation));
}

/// The page asked for an animation frame.
pub(crate) fn note_frame_request() {
    with_state_mut(|state| state.frames_owed = true);
}

pub(crate) fn is_ready() -> bool {
    with_state(|state| state.ready)
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

/// Which document is lent to this isolate right now, if any.
pub(crate) fn lent_document() -> Option<DocumentId> {
    LOANED.with(|slot| slot.borrow().as_ref().map(Document::id))
}

fn detached() -> JsError {
    JsError::Type(
        "this node belongs to a document that is not available to script right now".to_owned(),
    )
}
