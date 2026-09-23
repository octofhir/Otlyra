//! Scrolling: the page's own offset, the panels inside it, and their scrollbars.
//!
//! A scroll changes which part of the page is on screen and not where anything is,
//! so nothing here relays out or reshapes: it moves offsets and records paint. Its
//! own module because the wheel, a dragged scrollbar, *scroll into view* and
//! keeping the reader's place across a relayout are all answers to one question —
//! which part of the page is in the window.

use otlyra_layout::{BoxId, Damage};

use super::PageScene;

impl PageScene {
    /// Put the reader back where they were, as a reload does.
    ///
    /// Not clamped here: the new document may be shorter or taller, and the clamp
    /// happens on the next scroll or the next frame, once there is a layout to
    /// clamp against.
    pub fn set_scroll(&mut self, scroll: f32) {
        self.scroll = scroll.max(0.0);
        self.damage.add(Damage::PAINT);
    }

    /// Draw no scrollbars, for a picture that is going to be compared with one
    /// from elsewhere.
    pub fn hide_scrollbars(&mut self) {
        self.scrollbars = false;
    }

    /// Remember the place at the top of the window, to be put back after the
    /// next relayout.
    ///
    /// What browsers call scroll anchoring, and what a reader means by *keep my
    /// place*: the pixel offset is meaningless across a relayout, and the words
    /// at the top of the window are not.
    pub fn hold_the_reader_s_place(&mut self) {
        let Some((_, tree)) = self.layout.as_ref() else {
            return;
        };
        self.anchor = otlyra_layout::selection::position_at(tree, 0.0, self.scroll);
    }

    /// Put the page back where the anchor is, once it has been laid out again.
    pub(super) fn restore_the_reader_s_place(&mut self) {
        let Some(anchor) = self.anchor.take() else {
            return;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return;
        };
        let Some(rect) = otlyra_layout::selection::caret_rect(tree, anchor) else {
            return;
        };
        let content = tree.content_height();
        let max = (content - self.viewport_height).max(0.0);
        self.scroll = rect.y.clamp(0.0, max);
    }

    /// Take hold of a scrollbar under (`x`, `y`), if one is there.
    ///
    /// Returns whether it grabbed anything: a press that lands on a scrollbar
    /// belongs to it and not to the page behind it.
    pub fn grab_scrollbar(&mut self, x: f32, y: f32, width: f32, height: f32) -> bool {
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };

        // The page's own bar first: it is drawn over everything, so it is grabbed
        // before anything under it.
        let page_area = otlyra_layout::fragment::Rect::new(0.0, 0.0, width, height);
        if let Some(thumb) =
            otlyra_paint::scrollbar_thumb(page_area, tree.content_height(), self.scroll)
            && contains(thumb, x, y)
        {
            self.drag = Some(Drag {
                target: None,
                grabbed_at: y - thumb.y,
            });
            return true;
        }

        for port in &tree.scroll_ports {
            let mut area = port.port;
            area.y -= self.scroll;
            let at = self.port_scroll.get(&port.id).copied().unwrap_or(0.0);
            if let Some(thumb) = otlyra_paint::scrollbar_thumb(area, port.content_height, at)
                && contains(thumb, x, y)
            {
                self.drag = Some(Drag {
                    target: Some(port.id),
                    grabbed_at: y - thumb.y,
                });
                return true;
            }
        }

        false
    }

    /// Whether a scrollbar is being dragged.
    pub fn dragging_scrollbar(&self) -> bool {
        self.drag.is_some()
    }

    /// Let go of whatever was grabbed.
    pub fn release_scrollbar(&mut self) {
        self.drag = None;
    }

    /// Drag the grabbed scrollbar to `y`.
    ///
    /// The thumb follows the pointer and the content follows the thumb, which is
    /// the way round that makes a drag feel attached to the hand rather than to the
    /// document.
    pub fn drag_scrollbar(&mut self, y: f32, width: f32, height: f32) {
        let Some(drag) = self.drag else {
            return;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return;
        };

        let (area, content, range) = match drag.target {
            None => {
                let area = otlyra_layout::fragment::Rect::new(0.0, 0.0, width, height);
                let content = tree.content_height();
                (area, content, (content - height).max(0.0))
            }
            Some(id) => {
                let Some(port) = tree.scroll_ports.iter().find(|port| port.id == id) else {
                    return;
                };
                let mut area = port.port;
                area.y -= self.scroll;
                (area, port.content_height, port.range())
            }
        };

        let travel = otlyra_paint::scrollbar_travel(area, content);
        if travel <= 0.0 {
            return;
        }
        let wanted = ((y - drag.grabbed_at - area.y) / travel).clamp(0.0, 1.0) * range;

        match drag.target {
            None => self.set_scroll(wanted),
            Some(id) => {
                self.port_scroll.insert(id, wanted);
                self.damage.add(Damage::PAINT);
            }
        }
    }

    /// Scroll whatever is under (`x`, `y`) by `delta` logical pixels.
    ///
    /// A box that cuts its contents off and has more of them than it can show takes
    /// the wheel before the page does, and hands it back once it has reached its
    /// end — which is what makes a scrollable panel inside a page feel right rather
    /// than trapping the reader in it.
    pub fn scroll_at(&mut self, x: f32, y: f32, delta: f32) {
        let page_point = (x, y + self.scroll);
        let port = self.layout.as_ref().and_then(|(_, tree)| {
            // Innermost last: a port inside a port is pushed after it.
            tree.scroll_ports
                .iter()
                .rev()
                .find(|port| {
                    let offset = self.port_scroll.get(&port.id).copied().unwrap_or(0.0);
                    let _ = offset;
                    let rect = port.port;
                    page_point.0 >= rect.x
                        && page_point.0 < rect.right()
                        && page_point.1 >= rect.y
                        && page_point.1 < rect.bottom()
                })
                .copied()
        });

        if let Some(port) = port {
            let at = self.port_scroll.entry(port.id).or_insert(0.0);
            let wanted = *at + delta;
            let clamped = wanted.clamp(0.0, port.range());
            if (clamped - *at).abs() > f32::EPSILON {
                *at = clamped;
                self.damage.add(Damage::PAINT);
                return;
            }
            // At its end: the page takes the rest, rather than the wheel doing
            // nothing at all.
        }

        self.scroll_by(delta);
    }

    /// Scroll the page until `rect` is on screen, and answer whether it moved.
    ///
    /// The least that will do it, which is what every *scroll into view* means: a
    /// rectangle already in sight is left where it is, one above the top is
    /// brought to the top and one below the bottom to the bottom, each with a
    /// margin so that it does not end up against the edge with nothing around it.
    /// A rectangle taller than the window is aligned at its top, because the top
    /// of a thing is where reading it starts.
    ///
    /// The page's own scroll and no more. A rectangle inside a box that scrolls
    /// is brought as far into view as moving the page can bring it, and the box
    /// itself is not scrolled — which needs the rectangle in the box's own
    /// coordinates rather than the page's, and is not what this knows.
    pub fn scroll_to_rect(&mut self, rect: otlyra_layout::Rect) -> bool {
        /// How much room is left around what is scrolled to.
        const MARGIN: f32 = 24.0;

        let height = self.viewport_height;
        if height <= 0.0 {
            return false;
        }
        let top = rect.y - MARGIN;
        let bottom = rect.bottom() + MARGIN;
        let wanted = if top < self.scroll {
            top
        } else if bottom > self.scroll + height {
            if bottom - top > height {
                top
            } else {
                bottom - height
            }
        } else {
            return false;
        };

        let content = self
            .layout
            .as_ref()
            .map_or(0.0, |(_, tree)| tree.content_height());
        let scroll = wanted.clamp(0.0, (content - height).max(0.0));
        if (scroll - self.scroll).abs() < f32::EPSILON {
            return false;
        }
        self.scroll = scroll;
        self.damage.add(Damage::PAINT);
        true
    }

    /// Scroll the page by `delta` logical pixels, clamped to the content.
    ///
    /// Damages paint and no more: where the content is has not changed, only which
    /// part of it is on screen.
    pub fn scroll_by(&mut self, delta: f32) {
        self.damage.add(Damage::PAINT);
        let content = self
            .layout
            .as_ref()
            .map_or(0.0, |(_, tree)| tree.content_height());
        let max = (content - self.viewport_height).max(0.0);
        self.scroll = (self.scroll + delta).clamp(0.0, max);
    }
}

/// A scrollbar being dragged.
#[derive(Copy, Clone, Debug)]
pub(super) struct Drag {
    /// Which scroll port's bar, or the page's own.
    target: Option<BoxId>,
    /// Where on the thumb it was taken hold of, so it does not jump to the pointer.
    grabbed_at: f32,
}

/// Whether a rectangle contains a point.
fn contains(rect: otlyra_layout::fragment::Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}
