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

/// Actions a screen reader can ask for — clicking, focusing, scrolling.
///
/// Accepted and ignored for now: routing them needs the same input path a real
/// pointer takes, and that lands with keyboard focus in the page. Refusing to
/// create the adapter over it would be worse, because the tree itself is already
/// useful for reading.
struct IgnoreActions;

impl ActionHandler for IgnoreActions {
    fn do_action(&mut self, request: ActionRequest) {
        tracing::debug!(action = ?request.action, "accessibility action ignored");
    }
}

impl DeactivationHandler for IgnoreActions {
    fn deactivate_accessibility(&mut self) {}
}

/// Wraps the platform adapter.
pub(crate) struct Accessibility {
    adapter: accesskit_winit::Adapter,
    tree: SharedTree,
}

impl Accessibility {
    /// Start the adapter for `window`, which must not yet be visible.
    pub(crate) fn new(event_loop: &winit::event_loop::ActiveEventLoop, window: &Window) -> Self {
        let tree = SharedTree::default();
        Self {
            adapter: accesskit_winit::Adapter::with_direct_handlers(
                event_loop,
                window,
                tree.clone(),
                IgnoreActions,
                IgnoreActions,
            ),
            tree,
        }
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
