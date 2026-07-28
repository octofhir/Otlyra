//! One wrapper object per node, for as long as script can still reach it.
//!
//! ## Why a table
//!
//! A wrapper is a `NodeId` and a `DocumentId` — see [`super::node`] — and
//! nothing stops two of them naming the same node. Made afresh on every access
//! they are two JavaScript objects, and a page notices immediately:
//! `el === el` is false, `Set`s of elements fill up with duplicates, and
//! anything a page hangs *on* the wrapper — which in this browser is every event
//! listener — is hung on an object nobody will be handed again.
//!
//! So a node's wrapper is made once and remembered. This is the plan's §8.4 and
//! it is what every engine does: Chromium keeps the same table under the name
//! `DOMWrapperMap`, Gecko under `nsWrapperCache`.
//!
//! ## Why the roots are weak
//!
//! A strong root would make the table an owner: every element script ever
//! touched would be kept alive by a browser that has no other reason to hold it,
//! for as long as the page is open. Weak roots let the collector take a wrapper
//! nobody is holding, which is right — a wrapper carries no state of its own,
//! and the next access simply makes another.
//!
//! That is also why a cleared entry is not a bug to report but a lookup that
//! misses: [`Wrapped`] drops it and makes a new wrapper.
//!
//! ## What keeps it honest
//!
//! The key carries the document, so a wrapper from one page can never be handed
//! to another sharing the thread — the same rule the wrappers themselves follow.
//! The `NodeId` is generational, so a node that has been destroyed and its slot
//! reused is a different key and gets a different wrapper.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use otlyra_dom::{DocumentId, NodeId};
use otter_runtime::RuntimePersistentRootId as PersistentRootId;
use otter_runtime::marshal::{
    HostAncestry, HostClassMeta, IntoJs, JsError, JsValue, MarshalCx, class_instance,
};

thread_local! {
    /// The wrapper each node has been given, if it still has one.
    static WRAPPERS: RefCell<HashMap<(DocumentId, NodeId), PersistentRootId>> =
        RefCell::new(HashMap::new());
    /// How many entries the table had when it was last swept.
    static SWEPT_AT: Cell<usize> = const { Cell::new(0) };
}

/// How much the table may grow between sweeps.
///
/// A node that is removed from the tree and never asked about again leaves its
/// entry behind: the root goes cleared, but nothing looks at it, so nothing
/// notices. Left alone that is a table which only grows, on a page that
/// builds and throws away rows all day. The sweep costs one walk per this many
/// new wrappers, which is nothing beside the allocation of that many objects.
const SWEEP_EVERY: usize = 512;

/// Forget every wrapper this thread has handed out.
///
/// For a page that is being torn down or parsed again: the tree those wrappers
/// name is going, and an entry that outlived it would be a lookup that can only
/// miss. The roots themselves are weak and go with the isolate.
pub fn forget_all() {
    WRAPPERS.with(|wrappers| wrappers.borrow_mut().clear());
    SWEPT_AT.with(|at| at.set(0));
}

/// How many nodes currently have a wrapper. For tests and for the panel.
#[must_use]
pub fn wrapper_count() -> usize {
    WRAPPERS.with(|wrappers| wrappers.borrow().len())
}

/// A host class that is a name for one node.
///
/// Implemented by the four wrapper classes; it is what lets one table serve all
/// of them, since a node has exactly one wrapper whatever class that wrapper is.
pub trait NodeWrapper: HostAncestry + HostClassMeta + Sized {
    /// The node this names, and whose document.
    fn key(&self) -> (DocumentId, NodeId);
}

/// A wrapper on its way to script, through the table rather than around it.
///
/// A binding returns `Wrapped(element)` instead of `element` and that is the
/// whole of the difference: `IntoJs` looks the node up and hands back the object
/// it was given last time.
#[derive(Debug, Clone, Copy)]
pub struct Wrapped<T>(pub T);

impl<T: NodeWrapper> IntoJs for Wrapped<T> {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<JsValue<'s>, JsError> {
        let key = self.0.key();

        if let Some(root) = WRAPPERS.with(|wrappers| wrappers.borrow().get(&key).copied()) {
            match cx.ctx().persistent_root_get(root) {
                Some(value) => return Ok(cx.park(value)),
                // The collector took it, which is what a weak root is for. The
                // entry is dropped here rather than anywhere else, because this
                // is the only moment we are told.
                None => {
                    WRAPPERS.with(|wrappers| wrappers.borrow_mut().remove(&key));
                }
            }
        }

        let instance = class_instance(cx, T::JS_NAME, self.0)?;
        let value = cx.escape(instance);
        // Weak, and inserted before the handle is returned: the insertion may
        // allocate, and a `Local` is what survives an allocation — the raw value
        // above is only good until one happens, which is why it is escaped for
        // this call and not kept.
        let root = cx
            .ctx()
            .persistent_root_insert_weak(value)
            .map_err(|error| JsError::Type(error.to_string()))?;
        let grown = WRAPPERS.with(|wrappers| {
            let mut wrappers = wrappers.borrow_mut();
            wrappers.insert(key, root);
            wrappers.len()
        });
        if grown >= SWEPT_AT.with(Cell::get) + SWEEP_EVERY {
            sweep(cx);
        }
        Ok(instance)
    }
}

/// Drop the entries whose wrapper the collector has taken.
///
/// A lookup drops a cleared entry when it happens on one, but a node the page
/// has forgotten is a node nothing looks up — so the entries of a removed
/// subtree would sit there for the life of the page. The root is removed from
/// the engine's table too, which is the slot the plan's acceptance criterion is
/// about: the table must not grow across create-and-destroy cycles.
fn sweep(cx: &mut MarshalCx<'_, '_, '_>) {
    let dead: Vec<((DocumentId, NodeId), PersistentRootId)> = WRAPPERS.with(|wrappers| {
        wrappers
            .borrow()
            .iter()
            .filter(|(_, root)| cx.ctx().persistent_root_get(**root).is_none())
            .map(|(key, root)| (*key, *root))
            .collect()
    });

    for (key, root) in &dead {
        cx.ctx().persistent_root_remove(*root);
        WRAPPERS.with(|wrappers| wrappers.borrow_mut().remove(key));
    }

    let left = WRAPPERS.with(|wrappers| wrappers.borrow().len());
    SWEPT_AT.with(|at| at.set(left));
    if !dead.is_empty() {
        tracing::debug!(
            target: "page.script",
            dropped = dead.len(),
            kept = left,
            "swept the wrappers the collector took"
        );
    }
}
