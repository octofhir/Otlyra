//! The accessibility tree, handed to the platform's assistive technology.
//!
//! winit has no accessibility of its own — issue #1878 has been open since 2021 —
//! so this is AccessKit's adapter for the system: NSAccessibility on macOS, UI
//! Automation on Windows, attached to the window by its native handle. Linux has
//! none yet. What goes *into* the tree is the browser's business: a page's
//! structure lives in its DOM and no toolkit can guess it.

use std::sync::{Arc, Mutex};

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use raw_window_handle::RawWindowHandle;

/// The most recent tree, shared with the adapter.
///
/// Assistive technology can attach at any moment, and when it does the adapter
/// asks for the tree from whichever thread it is on. Keeping the last one here
/// means that question always has an answer, rather than one that depends on
/// catching the next frame.
#[derive(Clone, Default)]
pub(crate) struct SharedTree(Arc<Mutex<Option<RetainedTree>>>);

struct RetainedTree {
    tree: accesskit_consumer::Tree,
    info: accesskit::TreeInfo,
}

struct IgnoreChanges;
impl accesskit_consumer::TreeChangeHandler for IgnoreChanges {
    fn node_added(&mut self, _: &accesskit_consumer::NodeRef) {}
    fn node_updated(&mut self, _: &accesskit_consumer::NodeRef, _: &accesskit_consumer::NodeRef) {}
    fn focus_moved(
        &mut self,
        _: Option<&accesskit_consumer::NodeRef>,
        _: Option<&accesskit_consumer::NodeRef>,
    ) {
    }
    fn node_removed(&mut self, _: &accesskit_consumer::NodeRef) {}
}

impl SharedTree {
    fn set(&self, update: TreeUpdate) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(retained) = slot.as_mut() {
                if let Some(info) = &update.tree {
                    retained.info.clone_from(info);
                }
                retained
                    .tree
                    .update_and_process_changes(update, &mut IgnoreChanges);
            } else {
                let info = update
                    .tree
                    .clone()
                    .expect("initial accessibility tree has a root");
                *slot = Some(RetainedTree {
                    tree: accesskit_consumer::Tree::new(update, true),
                    info,
                });
            }
        }
    }
}

impl ActivationHandler for SharedTree {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        let slot = self.0.lock().ok()?;
        let retained = slot.as_ref()?;
        let state = retained.tree.state();
        let mut pending = vec![retained.info.root];
        let mut nodes = Vec::new();
        while let Some(id) = pending.pop() {
            let node = state.node_by_tree_local_id(id, accesskit::TreeId::ROOT)?;
            pending.extend_from_slice(node.data().children());
            nodes.push((id, node.data().clone()));
        }
        Some(TreeUpdate {
            nodes,
            tree: Some(retained.info.clone()),
            tree_id: accesskit::TreeId::ROOT,
            focus: state.locate_node(state.focus_id_in_tree())?.0,
        })
    }
}

/// Actions a screen reader asks for.
///
/// The adapter calls this from whichever thread the assistive technology is on,
/// so the request is queued rather than acted on: everything that changes the
/// browser happens on the loop's thread, which is the same rule a `Waker`
/// follows. The loop drains this after each batch of window events and delivers
/// what it found as an ordinary [`PlatformEvent`].
#[derive(Clone, Default)]
pub(crate) struct Actions(Arc<Mutex<Vec<(accesskit::NodeId, crate::AccessibilityAction)>>>);

impl Actions {
    /// Take everything asked for since the last time this was called.
    pub(crate) fn take(&self) -> Vec<(accesskit::NodeId, crate::AccessibilityAction)> {
        match self.0.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => Vec::new(),
        }
    }
}

impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        // The four a page can answer without a script: press this, put the
        // keyboard here, and move a slider either way. The rest name things this
        // browser has no node for yet, and logging the ones dropped is what makes
        // a missing one findable rather than mysterious.
        let action = match request.action {
            accesskit::Action::Click => crate::AccessibilityAction::Activate,
            accesskit::Action::Focus => crate::AccessibilityAction::Focus,
            accesskit::Action::Increment => crate::AccessibilityAction::Increment,
            accesskit::Action::Decrement => crate::AccessibilityAction::Decrement,
            other => {
                tracing::debug!(action = ?other, "accessibility action ignored");
                return;
            }
        };
        if let Ok(mut queue) = self.0.lock() {
            queue.push((request.target_node, action));
        }
    }
}

/// The system's adapter, attached to one window.
#[cfg(target_os = "macos")]
type Adapter = accesskit_macos::SubclassingAdapter;
#[cfg(target_os = "windows")]
type Adapter = accesskit_windows::SubclassingAdapter;

/// Attach an adapter to the window `handle` names.
#[cfg(target_os = "macos")]
fn attach(handle: RawWindowHandle, tree: SharedTree, actions: Actions) -> Option<Adapter> {
    let RawWindowHandle::AppKit(handle) = handle else {
        return None;
    };
    // SAFETY: the view is winit's, alive for as long as its window is, and the
    // adapter is dropped before the window (see `WindowedApp`). The adapter
    // subclasses the view in place, which is what AccessKit asks of a view it
    // does not own.
    Some(unsafe { Adapter::new(handle.ns_view.as_ptr(), tree, actions) })
}

#[cfg(target_os = "windows")]
fn attach(handle: RawWindowHandle, tree: SharedTree, actions: Actions) -> Option<Adapter> {
    let RawWindowHandle::Win32(handle) = handle else {
        return None;
    };
    let hwnd = accesskit_windows::HWND(handle.hwnd.get() as *mut std::ffi::c_void);
    Some(Adapter::new(hwnd, tree, actions))
}

/// The adapter, where the system has one, and what it hands the browser.
pub(crate) struct Accessibility {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    adapter: Option<Adapter>,
    tree: SharedTree,
    actions: Actions,
}

impl Accessibility {
    /// Start the adapter for the window `handle` names, which must not yet be
    /// visible.
    pub(crate) fn new(handle: RawWindowHandle) -> Self {
        let tree = SharedTree::default();
        let actions = Actions::default();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let adapter = attach(handle, tree.clone(), actions.clone());
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = handle;
        Self {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            adapter,
            tree,
            actions,
        }
    }

    /// Everything a reader has asked to press since this was last called.
    pub(crate) fn take_actions(&self) -> Vec<(accesskit::NodeId, crate::AccessibilityAction)> {
        self.actions.take()
    }

    /// Tell the adapter whether the window has the keyboard focus, which is
    /// how it learns to publish anything at all.
    pub(crate) fn focus_changed(&mut self, focused: bool) {
        #[cfg(target_os = "macos")]
        if let Some(events) = self
            .adapter
            .as_mut()
            .and_then(|adapter| adapter.update_view_focus_state(focused))
        {
            events.raise();
        }
        // Windows follows the window's focus itself.
        #[cfg(not(target_os = "macos"))]
        let _ = focused;
    }

    /// Publish a new tree.
    pub(crate) fn update(&mut self, update: TreeUpdate) {
        self.tree.set(update.clone());
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(events) = self
            .adapter
            .as_mut()
            .and_then(|adapter| adapter.update_if_active(|| update))
        {
            events.raise();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit::{Node, NodeId, Role, TreeId, TreeInfo};

    fn initial() -> TreeUpdate {
        let mut root = Node::new(Role::Window);
        root.set_children(vec![NodeId(1), NodeId(2)]);
        let mut field = Node::new(Role::TextInput);
        field.set_value("before");
        TreeUpdate {
            nodes: vec![
                (NodeId(0), root),
                (NodeId(1), field),
                (NodeId(2), Node::new(Role::Document)),
            ],
            tree: Some(TreeInfo::new(NodeId(0))),
            tree_id: TreeId::ROOT,
            focus: NodeId(1),
        }
    }

    #[test]
    fn late_activation_gets_the_complete_tree_after_partial_and_focus_updates() {
        let mut shared = SharedTree::default();
        assert!(shared.request_initial_tree().is_none());
        shared.set(initial());
        let mut changed = Node::new(Role::TextInput);
        changed.set_value("after");
        shared.set(TreeUpdate {
            nodes: vec![(NodeId(1), changed)],
            tree: None,
            tree_id: TreeId::ROOT,
            focus: NodeId(1),
        });
        shared.set(TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: TreeId::ROOT,
            focus: NodeId(2),
        });
        let complete = shared.request_initial_tree().unwrap();
        assert_eq!(complete.nodes.len(), 3);
        assert_eq!(complete.focus, NodeId(2));
        assert_eq!(
            complete
                .nodes
                .iter()
                .find(|(id, _)| *id == NodeId(1))
                .unwrap()
                .1
                .value(),
            Some("after")
        );
        let tree = accesskit_consumer::Tree::new(complete, true);
        assert_eq!(tree.state().focus_in_tree().data().role(), Role::Document);
    }

    #[test]
    fn activation_omits_removed_children_and_keeps_tree_metadata() {
        let mut shared = SharedTree::default();
        shared.set(initial());
        let mut root = Node::new(Role::Window);
        root.set_children(vec![NodeId(2)]);
        let mut info = TreeInfo::new(NodeId(0));
        info.toolkit_name = Some("Otlyra".into());
        shared.set(TreeUpdate {
            nodes: vec![(NodeId(0), root)],
            tree: Some(info.clone()),
            tree_id: TreeId::ROOT,
            focus: NodeId(2),
        });
        let complete = shared.request_initial_tree().unwrap();
        assert_eq!(complete.nodes.len(), 2);
        assert!(!complete.nodes.iter().any(|(id, _)| *id == NodeId(1)));
        assert_eq!(complete.tree, Some(info));
        accesskit_consumer::Tree::new(complete, true);
    }
}
