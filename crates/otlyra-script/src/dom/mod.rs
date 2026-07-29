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

use std::cell::{Cell, RefCell};

use otlyra_dom::{Document, DocumentId, NodeId};
use otter_runtime::marshal::JsError;

pub use identity::{Wrapped, forget_all as forget_wrappers, wrapper_count};
pub use node::{DOM_EXTENSION, DocumentRef, ElementRef, NodeRef, TextRef};

thread_local! {
    /// The document this thread's isolate is currently allowed to touch.
    static LOANED: RefCell<Option<Document>> = const { RefCell::new(None) };
    /// Whether script has changed the document since the flag was last read.
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    /// Whether the parser has finished, which is what `readyState` reports.
    static READY: Cell<bool> = const { Cell::new(false) };
}

/// Somewhere a page's script asked to go.
///
/// Script cannot navigate: it can only say so, and the browser decides. That is
/// not politeness — the isolate holds the document for the length of one turn
/// and the navigation replaces the document, so a binding that navigated where
/// it stands would be destroying the thing it is standing on.
#[derive(Debug, Clone)]
pub enum Navigation {
    /// `location.href = …`, `location.assign`, `location.replace`.
    Url {
        /// Where to, as the page spelled it. Resolving it against the
        /// document's own address is the browser's.
        href: String,
        /// Whether this replaces the current history entry.
        replace: bool,
    },
    /// `form.submit()`.
    Submit {
        /// The `<form>` element. Its fields and its `action` are read from the
        /// document, which is where they are.
        form: NodeId,
    },
    /// `location.reload()`.
    Reload,
}

// Where the page's script thinks it is, and where it asked to go.
thread_local! {
    static DOCUMENT_URL: RefCell<String> = const { RefCell::new(String::new()) };
    static NAVIGATION: RefCell<Option<Navigation>> = const { RefCell::new(None) };
}

/// Tell the isolate what this document's address is.
///
/// `location` is built from it, and a relative navigation is resolved against
/// it by the browser afterwards.
pub fn set_document_url(url: impl Into<String>) {
    DOCUMENT_URL.with(|slot| *slot.borrow_mut() = url.into());
}

pub(crate) fn document_url() -> String {
    DOCUMENT_URL.with(|slot| slot.borrow().clone())
}

pub(crate) fn request_navigation(navigation: Navigation) {
    // Last one wins. A script that sets `location.href` twice in a turn has
    // changed its mind, and a browser goes where it ended up.
    NAVIGATION.with(|slot| *slot.borrow_mut() = Some(navigation));
}

/// Where script asked to go, if it asked.
///
/// Read after a turn, by whoever is able to navigate.
pub fn take_navigation() -> Option<Navigation> {
    NAVIGATION.with(|slot| slot.borrow_mut().take())
}

thread_local! {
    /// How many animation frames this page has asked for and not been given.
    ///
    /// Kept on this side so a browser can ask *whether a frame is owed* without
    /// entering the isolate. A frame is drawn sixty times a second and almost
    /// none of them are owed a callback; a turn per frame to find that out is a
    /// turn per frame for nothing.
    static FRAMES: Cell<u64> = const { Cell::new(0) };
}

/// The page asked for an animation frame.
pub(crate) fn note_frame_request() {
    FRAMES.with(|frames| frames.set(frames.get().saturating_add(1)));
}

/// Whether any are outstanding.
#[must_use]
pub fn frames_pending() -> bool {
    FRAMES.with(Cell::get) > 0
}

/// Forget them: the frame that would have run them is being run now, or the
/// page they belong to is going.
pub fn clear_frame_requests() {
    FRAMES.with(|frames| frames.set(0));
}

/// Say whether the document has finished parsing.
///
/// One bit, because that is all `readyState` is worth until there is a real
/// document lifecycle: everything before the last byte is `"loading"` and
/// everything after it is `"complete"`.
pub fn set_ready(ready: bool) {
    READY.with(|flag| flag.set(ready));
}

pub(crate) fn is_ready() -> bool {
    READY.with(Cell::get)
}

/// Lend `document` to the isolate for the duration of `run`.
///
/// The document is moved in and moved back; a caller still holding `&mut
/// Document` cannot reach it while script can, which is the whole point.
///
/// Loans do not nest. A second one while the first is live would take an empty
/// slot, and script would run against a document with no nodes in it — so it
/// panics instead, on our own bug rather than on a page's.
pub fn loan<R>(document: &mut Document, run: impl FnOnce() -> R) -> R {
    let taken = std::mem::take(document);
    LOANED.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "a document is already lent to this isolate");
        *slot = Some(taken);
    });

    // The restoring is a `Drop` so that a panicking script — or a native that
    // unwinds — still gives the document back rather than leaving the page
    // holding an empty one.
    struct Restore<'a> {
        document: &'a mut Document,
    }

    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            if let Some(document) = LOANED.with(|slot| slot.borrow_mut().take()) {
                *self.document = document;
            }
        }
    }

    let _restore = Restore { document };
    run()
}

/// Whether script changed the document, clearing the flag.
pub fn take_dirty() -> bool {
    DIRTY.with(|dirty| dirty.replace(false))
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
            DIRTY.with(|dirty| dirty.set(true));
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
