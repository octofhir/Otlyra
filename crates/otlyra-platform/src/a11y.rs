//! The accessibility tree, handed to the platform's assistive technology.
//!
//! winit has no accessibility of its own — issue #1878 has been open since 2021 —
//! so this is `accesskit_winit`, which speaks NSAccessibility on macOS, UI
//! Automation on Windows and AT-SPI on Linux. What goes *into* the tree is the
//! browser's business: a page's structure lives in its DOM and no toolkit can
//! guess it.

use std::sync::{Arc, Mutex};

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, TreeUpdate};
use winit::window::Window;

/// The most recent tree, shared with the adapter.
///
/// Assistive technology can attach at any moment, and when it does the adapter
/// asks for the tree from whichever thread it is on. Keeping the last one here
/// means that question always has an answer, rather than one that depends on
/// catching the next frame.
#[derive(Clone, Default)]
pub(crate) struct SharedTree(Arc<Mutex<Option<TreeUpdate>>>);

impl SharedTree {
    fn set(&self, update: TreeUpdate) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(update);
        }
    }
}

impl ActivationHandler for SharedTree {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.0.lock().ok()?.clone()
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
        // The two a page can answer without a script: press this, and put the
        // keyboard here. The rest name things this browser has no node for yet,
        // and logging the ones dropped is what makes a missing one findable
        // rather than mysterious.
        let action = match request.action {
            accesskit::Action::Click => crate::AccessibilityAction::Activate,
            accesskit::Action::Focus => crate::AccessibilityAction::Focus,
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

impl DeactivationHandler for Actions {
    fn deactivate_accessibility(&mut self) {}
}

/// Wraps the platform adapter.
pub(crate) struct Accessibility {
    adapter: accesskit_winit::Adapter,
    tree: SharedTree,
    actions: Actions,
}

impl Accessibility {
    /// Start the adapter for `window`, which must not yet be visible.
    pub(crate) fn new(event_loop: &winit::event_loop::ActiveEventLoop, window: &Window) -> Self {
        let tree = SharedTree::default();
        let actions = Actions::default();
        Self {
            adapter: accesskit_winit::Adapter::with_direct_handlers(
                event_loop,
                window,
                tree.clone(),
                actions.clone(),
                actions.clone(),
            ),
            tree,
            actions,
        }
    }

    /// Everything a reader has asked to press since this was last called.
    pub(crate) fn take_actions(&self) -> Vec<(accesskit::NodeId, crate::AccessibilityAction)> {
        self.actions.take()
    }

    /// Let the adapter see a window event. Focus changes in particular are how it
    /// learns to publish anything at all.
    pub(crate) fn process_event(&mut self, window: &Window, event: &winit::event::WindowEvent) {
        self.adapter.process_event(window, event);
    }

    /// Publish a new tree.
    pub(crate) fn update(&mut self, update: TreeUpdate) {
        self.tree.set(update.clone());
        self.adapter.update_if_active(|| update);
    }
}
