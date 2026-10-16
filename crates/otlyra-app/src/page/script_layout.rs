//! The layout script measures with when it measures after a change.
//!
//! A page that inserts a dialog and then centres it reads the dialog's size
//! in the same turn (CSSOM View: measuring flushes layout). The last frame
//! knows nothing of the dialog, so the page is answered from a layout of the
//! tree as script left it: styled with the sheets the page has, boxed with
//! the pictures it has, laid out at the width the page was last laid out at,
//! on a fork of the browser's text engine.
//!
//! A full layout is not cheap on a large page, and some pages measure after
//! every change. Script may spend up to [`BURST`] on these layouts at once,
//! and earns more back at a quarter of the time that passes; once it has
//! spent it, the last answer stands until it has earned enough again, as it
//! would between two frames. Whatever a page does, layouts for script take
//! at most about a fifth of its time.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use otlyra_dom::{Document, NodeId};
use otlyra_layout::Images;
use otlyra_text::TextEngine;

/// The most layout time script may spend in one go.
const BURST: Duration = Duration::from_millis(100);
/// How much layout time each unit of time that passes earns back.
const REFILL: f64 = 0.25;

pub(crate) struct ScriptLayout {
    styler: Rc<otlyra_css::cascade::ScriptStyler>,
    text: RefCell<TextEngine>,
    images: Rc<RefCell<Images>>,
    /// The width and height the page was last laid out at.
    size: Rc<Cell<(f32, f32)>>,
    /// The layout time script may still spend, and when that was worked out.
    budget: Cell<(Duration, Instant)>,
}

impl ScriptLayout {
    pub(crate) fn new(
        styler: Rc<otlyra_css::cascade::ScriptStyler>,
        text: TextEngine,
        images: Rc<RefCell<Images>>,
        size: Rc<Cell<(f32, f32)>>,
    ) -> Self {
        Self {
            styler,
            text: RefCell::new(text),
            images,
            size,
            budget: Cell::new((BURST, Instant::now())),
        }
    }
}

impl otlyra_dom::GeometryResolver for ScriptLayout {
    fn resolve(&self, document: &Document) -> Option<Arc<HashMap<NodeId, [f32; 4]>>> {
        let started = Instant::now();
        let (left, as_of) = self.budget.get();
        let left = (left + started.duration_since(as_of).mul_f64(REFILL)).min(BURST);
        self.budget.set((left, started));
        if left.is_zero() {
            return None;
        }
        let (width, height) = self.size.get();
        if width <= 0.0 {
            return None;
        }
        let styled = self.styler.styled(document)?;
        let images = self.images.try_borrow().ok()?;
        let mut boxes =
            otlyra_layout::build_page_box_tree(document, &styled, &images, document.form());
        drop(images);
        let mut text = self.text.try_borrow_mut().ok()?;
        let tree = otlyra_layout::layout(
            &mut boxes,
            &mut text,
            otlyra_layout::Viewport { width, height },
        );
        drop(text);
        let (scroll_x, scroll_y) = document.scroll_position();
        let mut rects: HashMap<NodeId, [f32; 4]> = HashMap::new();
        for fragment in tree.iter() {
            let Some(node) = fragment
                .box_id
                .and_then(|id| boxes.get(id))
                .and_then(|layout| layout.node)
            else {
                continue;
            };
            let (dx, dy) = if fragment.fixed {
                (0.0, 0.0)
            } else {
                (scroll_x as f32, scroll_y as f32)
            };
            let rect = fragment.rect;
            let next = [
                rect.x - dx,
                rect.y - dy,
                rect.x + rect.width - dx,
                rect.y + rect.height - dy,
            ];
            rects
                .entry(node)
                .and_modify(|union| {
                    union[0] = union[0].min(next[0]);
                    union[1] = union[1].min(next[1]);
                    union[2] = union[2].max(next[2]);
                    union[3] = union[3].max(next[3]);
                })
                .or_insert(next);
        }
        let took = started.elapsed();
        self.budget.set((left.saturating_sub(took), Instant::now()));
        tracing::debug!(target: "page.script", nodes = rects.len(), ?took, "laid out for script");
        Some(Arc::new(rects))
    }
}
