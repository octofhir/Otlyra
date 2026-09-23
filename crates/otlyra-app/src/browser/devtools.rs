//! The inspector as the browser drives it: the dock, the picker and the chosen box.
//!
//! The panel draws itself; what it cannot do is reach the page. Everything here is
//! that reaching — which element a point names, what layout made of its box, how
//! much of the window the dock takes — so the panel, the overlay and a driver all
//! read one account of it.

use std::sync::Arc;

use crate::page::PageScene;
use crate::ui::UI_HEIGHT;

use super::{Browser, SURFACE_INSPECTOR};

impl Browser {
    /// Show or hide developer tools and move keyboard ownership with the panel.
    pub(super) fn toggle_inspector(&mut self) {
        self.inspector.toggle();
        let surface = if self.inspector.open {
            SURFACE_INSPECTOR
        } else {
            self.tab_surface()
        };
        self.activate_surface(surface);
    }

    /// Apply what the inspector reported that the browser has to do about.
    ///
    /// Almost nothing: the panel settles its own state and reports `None` for
    /// it. Editing is the exception, because the panel does not hold the
    /// document — it says what to set and this is what sets it.
    pub(super) fn apply_inspector(&mut self, action: crate::inspector::Action) {
        let crate::inspector::Action::SetAttribute { name, value } = action else {
            return;
        };
        let Some(node) = self.inspector.selected else {
            return;
        };
        let Some(page) = self.tabs[self.active].page.as_mut() else {
            return;
        };
        // The edit, and then everything downstream of the document: a restyle, a
        // fresh box tree, a relayout. The selection is a node id and the node is
        // still the same node, so what was being looked at is still what is.
        if page.edit(|document| document.set_attr(node, &name, &value)) {
            tracing::info!(%name, "attribute set from the inspector");
        }
    }

    /// How much of a content area `height` tall the inspector takes.
    ///
    /// Only over a document: a browser page is the browser looked at from the
    /// front and has no DOM of its own to inspect, so the panel stays out of the
    /// way rather than showing an empty tree beside one.
    pub(super) fn dock_height(&self, height: f64) -> f64 {
        if self.tabs[self.active].system.is_some() {
            return 0.0;
        }
        self.inspector.dock_height(height)
    }

    /// The four shades of the chosen box, and its tracks if it has any.
    pub(super) fn paint_highlight(&mut self, list: &mut otlyra_gfx::DisplayList) {
        let Some(chosen) = self.chosen_box() else {
            return;
        };
        let theme = self.inspector.theme.clone();
        crate::inspector::paint_highlight(
            list,
            &theme,
            chosen.border,
            &chosen.edges,
            chosen.tracks.is_none(),
        );
        if let Some(tracks) = chosen.tracks.as_ref() {
            let mut cx = crate::widget::Cx::new(&mut self.text);
            cx.theme = theme;
            crate::inspector::paint_tracks(
                list,
                &mut cx,
                chosen.edges.content_of(chosen.border),
                tracks,
            );
        }
    }

    /// The panel below it, as the list the panel hands back.
    ///
    /// An `Arc` the panel keeps: while nothing it is drawn from has moved it is
    /// the same list frame after frame, which is what lets the layer above skip
    /// scaling it again.
    pub(super) fn inspector_panel(
        &mut self,
        width: f64,
        top: f64,
        content_height: f64,
        dock: f64,
    ) -> Arc<otlyra_gfx::DisplayList> {
        let chosen = self.chosen_box();
        let panel = crate::ui::Rect::new(0.0, top + content_height, width, dock);
        // Everything the panel is shown about the page, gathered before it is
        // built: the panel reads, and the browser is what does the reaching.
        let page = self.tabs[self.active].page.as_ref();
        let style = page.and_then(|page| {
            self.inspector
                .selected
                .and_then(|node| page.boxes().box_for(node))
                .and_then(|id| page.boxes().get(id))
                .map(|node| node.style.as_ref())
        });
        // Assembled whether or not the tab has a document: a load that failed
        // has a network list saying why, and hiding the panel behind a page
        // would hide the pane that explains the missing page.
        // Only for the pane that shows them: walking the rule chain for a node
        // nobody is looking at is work for a pane that is not open.
        let rules = match (self.inspector.sidebar, page, self.inspector.selected) {
            (crate::inspector::Sidebar::Rules, Some(page), Some(node)) => page.rules_for(node),
            _ => Vec::new(),
        };
        let facts = crate::inspector::Facts {
            document: page.map(PageScene::document),
            page,
            style,
            rules: &rules,
            rect: chosen.as_ref().map(|chosen| chosen.border),
            containing: chosen.as_ref().and_then(|chosen| chosen.containing),
            exchanges: self.fetcher.exchanges(),
        };
        self.inspector
            .build_display_list(panel, &facts, &mut self.text)
    }

    /// The inspector, for whoever is driving the browser rather than using it.
    ///
    /// The command line and the screenshot harness both need to open the panel
    /// and choose something in it, and neither has a pointer to do it with.
    pub fn inspector_mut(&mut self) -> &mut crate::inspector::Inspector {
        &mut self.inspector
    }

    /// Choose the element drawn at `x`, `y`, as the picker would.
    ///
    /// Tested against the last frame, like every other hit test here: a point
    /// can only be resolved against a frame that has been drawn.
    pub fn inspect_at(&mut self, x: f64, y: f64) {
        self.inspector.open = true;
        self.pick_at(x, y);
    }

    /// Everything about the chosen node's box that the panel and the overlay
    /// both need.
    ///
    /// The rectangle comes from the same targets a click is tested against, so
    /// the overlay lands exactly where the box did and no second answer to
    /// *where is this* exists.
    pub(super) fn chosen_box(&self) -> Option<Chosen> {
        self.box_facts(self.inspector.selected?)
    }

    /// The same, for any node rather than the chosen one.
    ///
    /// What a driver asks about: it names a node and wants what the engine made
    /// of it. The overlay and the panel ask through the chosen one, and all
    /// three go through here, so there is one account of what a box is.
    pub fn box_facts(&self, node: otlyra_dom::NodeId) -> Option<Chosen> {
        let page = self.tabs[self.active].page.as_ref()?;
        let id = page.boxes().box_for(node)?;
        let border = to_rect(page.rect_of(id)?);
        let box_node = page.boxes().get(id)?;
        let style = &box_node.style;

        // How wide the containing block is, for the percentages: the parent's
        // content box, worked out the same way this one's is.
        let containing = box_node
            .parent
            .and_then(|parent| Some((page.boxes().get(parent)?, page.rect_of(parent)?)))
            .map(|(parent, rect)| {
                crate::inspector::BoxEdges::of(&parent.style, None)
                    .content_of(to_rect(rect))
                    .width
            });
        // What layout actually gave it, and only failing that what the style
        // says. The used values are the ones a box model is asking about: a
        // computed `margin: auto` is not a number, and the number it came out as
        // is known to layout alone.
        let edges = page
            .used_edges(id)
            .map(crate::inspector::BoxEdges::used)
            .unwrap_or_else(|| crate::inspector::BoxEdges::of(style, containing));

        // A container whose children were laid out into tracks gets the dashed
        // overlay: the lines a stylesheet names are invisible until they are
        // drawn on the page they laid out.
        let tracks = matches!(
            style.display,
            otlyra_css::Display::Grid | otlyra_css::Display::Flex
        )
        .then(|| {
            let items: Vec<crate::ui::Rect> = box_node
                .children
                .iter()
                .filter_map(|child| page.rect_of(*child))
                .map(to_rect)
                .collect();
            crate::inspector::Tracks::of(
                edges.content_of(border),
                &items,
                style.display == otlyra_css::Display::Grid,
                (
                    f64::from(style.gap.0.resolve(border.width as f32)),
                    f64::from(style.gap.1.resolve(border.width as f32)),
                ),
            )
        });

        Some(Chosen {
            border,
            edges,
            containing,
            tracks,
        })
    }

    /// Choose the element drawn at `x`, `y`, and reveal it in the tree.
    ///
    /// The hit test is the page's own — the one a click is tested against — so
    /// the element the overlay names is the element a click would have hit.
    /// Nothing new is measured and no second answer to *what is here* exists.
    pub(super) fn pick_at(&mut self, x: f64, y: f64) {
        let (x, y) = self.in_page(x, y);
        let Some(page) = self.tabs[self.active].page.as_ref() else {
            return;
        };
        let Some(node) = page
            .box_at(x, y)
            .and_then(|id| page.boxes().get(id))
            // A box the parser never made a node for is an anonymous one the
            // layout invented. Its nearest real ancestor is what a person means
            // by "this element".
            .and_then(|node| node.node.or_else(|| self.nearest_node(page, node)))
        else {
            return;
        };
        let document = page.document();
        self.inspector.reveal(document, node);
    }

    /// The first node an anonymous box's ancestors carry.
    fn nearest_node(
        &self,
        page: &PageScene,
        node: &otlyra_layout::BoxNode,
    ) -> Option<otlyra_dom::NodeId> {
        let mut current = node.parent;
        while let Some(id) = current {
            let box_node = page.boxes().get(id)?;
            if let Some(node) = box_node.node {
                return Some(node);
            }
            current = box_node.parent;
        }
        None
    }

    /// Where the inspector's panel starts, or the bottom of the window when it
    /// is not showing.
    pub(super) fn dock_top(&self) -> f64 {
        let top = if self.interface { UI_HEIGHT } else { 0.0 };
        self.last_height - self.dock_height(self.last_height - top)
    }
}

/// One node's box, as the overlay, the panel and a driver all need it.
pub struct Chosen {
    /// The border box, in window coordinates.
    pub border: crate::ui::Rect,
    /// What the style says its four edges are.
    pub edges: crate::inspector::BoxEdges,
    /// How wide its containing block is, for a percentage.
    pub containing: Option<f64>,
    /// Where its children's tracks fall, when it lays its children into any.
    pub tracks: Option<crate::inspector::Tracks>,
}

/// A layout rectangle in the interface's own geometry vocabulary.
fn to_rect(rect: otlyra_layout::Rect) -> crate::ui::Rect {
    crate::ui::Rect::new(
        f64::from(rect.x),
        f64::from(rect.y),
        f64::from(rect.width),
        f64::from(rect.height),
    )
}
