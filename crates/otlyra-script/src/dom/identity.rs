//! One wrapper object per node, of the right interface, for as long as script
//! can still reach it.
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
//! ## Why one way in
//!
//! Every binding hands a node to script as a [`JsNode`], and [`JsNode`] alone
//! decides what the wrapper is made as: the node's kind picks the data the
//! wrapper carries, and an element's name picks its interface — see
//! [`super::interfaces`]. A wrapper made through one binding is therefore the
//! same object, of the same class, as one made through any other: a node first
//! met as somebody's `parentNode` is not an `Element` that lacks
//! `HTMLInputElement`'s members when it is later met by `querySelector`.
//!
//! ## Why the roots are strong
//!
//! A page keeps its own state *on* nodes: React hangs its fiber and its props
//! off every element it renders under keys like `__reactFiber$…`, jQuery its
//! data under its expando, and a hundred smaller libraries do the same. That
//! state is a property of the wrapper object, so a wrapper the collector took
//! would take it with it, and the next lookup would hand the page a fresh
//! object that has forgotten everything — "unable to find node on an unmounted
//! component", on a node that is plainly still there.
//!
//! So a wrapper lives as long as its node can be reached, and since the arena
//! does not free nodes yet, that is as long as the page. Every engine ties the
//! two lifetimes together by tracing from the tree; ours will when the arena
//! learns to collect detached nodes, and the table will learn it in the same
//! change. Until then the cost is one object per node script has touched,
//! which is the same order as the arena's own.
//!
//! ## What keeps it honest
//!
//! The key carries the document, so a wrapper from one page can never be handed
//! to another sharing the thread — the same rule the wrappers themselves follow.
//! The `NodeId` is generational, so a node that has been destroyed and its slot
//! reused is a different key and gets a different wrapper.

use std::cell::RefCell;
use std::collections::HashMap;

use otlyra_dom::{DocumentId, NodeId};
use otter_runtime::RuntimePersistentRootId as PersistentRootId;
use otter_runtime::marshal::{IntoJs, JsError, JsValue, MarshalCx, class_instance};

use super::node::{
    CharacterDataRef, CommentRef, DocumentFragmentRef, DocumentRef, DocumentTypeRef, ElementRef,
    NodeRef, TextRef,
};
use super::tree::{self, Kind};

/// Where the glue keeps each interface's own prototype — see `compat.js`.
const INTERFACE_PROTOTYPES: &str = "__otlyraInterfacePrototypes";

thread_local! {
    /// The wrapper each node has been given.
    static WRAPPERS: RefCell<HashMap<(DocumentId, NodeId), PersistentRootId>> =
        RefCell::new(HashMap::new());
}

/// Forget every wrapper this thread has handed out.
///
/// For a page that is being torn down or parsed again: the tree those wrappers
/// name is going, and an entry that outlived it would be a lookup that can only
/// miss. The roots go with the isolate they were made in.
pub fn forget_all() {
    WRAPPERS.with(|wrappers| wrappers.borrow_mut().clear());
}

/// How many nodes currently have a wrapper. For tests and for the panel.
#[must_use]
pub fn wrapper_count() -> usize {
    WRAPPERS.with(|wrappers| wrappers.borrow().len())
}

/// A node on its way to script, made as whatever kind of node it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsNode {
    /// Whose node.
    pub(crate) doc: DocumentId,
    /// Which node.
    pub(crate) id: NodeId,
}

impl JsNode {
    /// The node `id` of document `doc`.
    pub(crate) fn new(doc: DocumentId, id: NodeId) -> Self {
        Self { doc, id }
    }
}

impl IntoJs for JsNode {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<JsValue<'s>, JsError> {
        let key = (self.doc, self.id);

        if let Some(root) = WRAPPERS.with(|wrappers| wrappers.borrow().get(&key).copied()) {
            match cx.ctx().persistent_root_get(root) {
                Some(value) => return Ok(cx.park(value)),
                // A strong root does not clear; one that has is a root the
                // engine dropped with an isolate this table outlived, and the
                // entry is as good as absent.
                None => {
                    WRAPPERS.with(|wrappers| wrappers.borrow_mut().remove(&key));
                }
            }
        }

        // What to make it as is a question about the node, asked of the lent
        // document: its kind, and for an element its name.
        let made = super::with_document(self.doc, |document| {
            let kind = tree::kind(document, self.id)?;
            let interface = match kind {
                Kind::Element => document
                    .get(self.id)
                    .and_then(|node| node.element())
                    .map_or("Element", |element| {
                        super::interfaces::element_interface(&element.name)
                    }),
                Kind::Text => "Text",
                Kind::Comment => "Comment",
                Kind::Document => "HTMLDocument",
                Kind::Fragment => "DocumentFragment",
                Kind::Doctype => "DocumentType",
            };
            Some((kind, interface))
        })?;
        // A node that is gone by the time it is handed over — destroyed by the
        // turn that is handing it — is nothing, not an error.
        let Some((kind, interface)) = made else {
            return Ok(cx.null());
        };

        let node = NodeRef::of(self.doc, self.id);
        let instance = match kind {
            Kind::Element => class_instance(cx, interface, ElementRef { node })?,
            Kind::Text => class_instance(
                cx,
                interface,
                TextRef {
                    data: CharacterDataRef { node },
                },
            )?,
            Kind::Comment => class_instance(
                cx,
                interface,
                CommentRef {
                    data: CharacterDataRef { node },
                },
            )?,
            Kind::Document => class_instance(cx, interface, DocumentRef { node })?,
            Kind::Fragment => class_instance(cx, interface, DocumentFragmentRef { node })?,
            Kind::Doctype => class_instance(cx, interface, DocumentTypeRef { node })?,
        };
        // The realm's own prototype for the interface, which a page that put
        // something else at the global by its name has not changed.
        let global = cx.global_this();
        if let Some(prototypes) = cx.data_property(global, INTERFACE_PROTOTYPES)
            && let Some(prototype) = cx.data_property(prototypes, interface)
        {
            cx.set_prototype(instance, Some(prototype))?;
        }
        let value = cx.escape(instance);
        // Inserted before the handle is returned: the insertion may allocate,
        // and a `Local` is what survives an allocation — the raw value above is
        // only good until one happens, which is why it is escaped for this call
        // and not kept.
        let root = cx.ctx().persistent_root_insert(value);
        WRAPPERS.with(|wrappers| wrappers.borrow_mut().insert(key, root));
        Ok(instance)
    }
}
