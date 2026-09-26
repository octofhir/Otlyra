//! What each part of a page moves with when it scrolls, and how far.
//!
//! A display list is built with the page where layout put it; the scroll is
//! applied when the list is drawn, as one offset per [`SpatialId`]. That is what
//! lets a scroll be a new set of offsets rather than a new list. Which ids a list
//! uses, and what each one's offset is at a given scroll, are decided together
//! here — the walk that tags an item and the rule that moves it cannot disagree
//! about what the tag means.

use otlyra_gfx::kurbo::Vec2;
use otlyra_gfx::{DisplayList, SpatialId};
use otlyra_layout::fragment::{Rect, Sticky};

use crate::scrollbar::scrollbar_thumb;

/// The page itself: everything in the flow, and what is absolutely positioned in
/// it.
pub const PAGE: SpatialId = SpatialId(1);

/// The page's scrollbar thumb, which travels with the scroll at its own rate.
pub const PAGE_THUMB: SpatialId = SpatialId(2);

/// A painted page: the list, and what moves each part of it.
pub struct PageList {
    /// The items, in page coordinates for whatever scrolls with the page.
    pub list: DisplayList,
    /// How far each part of the list has moved at a given scroll.
    pub spatials: Spatials,
}

impl PageList {
    /// The list as plain geometry at `scroll_y`: every offset applied, nothing
    /// left to move.
    pub fn at(&self, scroll_y: f32) -> DisplayList {
        self.list.with_offsets(&self.spatials.offsets(scroll_y))
    }
}

/// One id's rule for how far it has moved.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Rule {
    /// Never moves.
    Viewport,
    /// Moves by the page's scroll.
    Page,
    /// The page's scrollbar thumb, drawn where it sits at no scroll.
    Thumb { area: Rect, content_height: f32 },
    /// A sticky box and everything in it: with the page, until its inset holds it,
    /// until its container runs out. `inside` is how far the scroll port it is in
    /// had been scrolled when the list was built, which is part of the scroll the
    /// box answers to.
    Sticky { sticky: Sticky, inside: f32 },
}

/// The rules for every id a list uses, in id order.
#[derive(Clone, Debug, PartialEq)]
pub struct Spatials {
    rules: Vec<Rule>,
    viewport_height: f32,
}

impl Spatials {
    /// The viewport, the page and its thumb, for a viewport `viewport` high.
    pub(crate) fn new(viewport_height: f32) -> Self {
        Self {
            rules: vec![Rule::Viewport, Rule::Page, Rule::Viewport],
            viewport_height,
        }
    }

    /// Say where the page's scrollbar thumb is drawn from.
    pub(crate) fn thumb(&mut self, area: Rect, content_height: f32) {
        self.rules[PAGE_THUMB.0 as usize] = Rule::Thumb {
            area,
            content_height,
        };
    }

    /// The id for a sticky box, made the first time it is asked for. Everything
    /// inside a sticky box carries the same constraint, so the whole box is one id.
    pub(crate) fn sticky(&mut self, sticky: Sticky, inside: f32) -> SpatialId {
        let rule = Rule::Sticky { sticky, inside };
        let index = match self.rules.iter().position(|known| *known == rule) {
            Some(index) => index,
            None => {
                self.rules.push(rule);
                self.rules.len() - 1
            }
        };
        SpatialId(index as u32)
    }

    /// How far each id has moved with the page scrolled to `scroll_y`, in id
    /// order: what [`otlyra_gfx::render_scrolled`] takes.
    pub fn offsets(&self, scroll_y: f32) -> Vec<Vec2> {
        self.rules
            .iter()
            .map(|rule| match *rule {
                Rule::Viewport => Vec2::ZERO,
                Rule::Page => Vec2::new(0.0, f64::from(scroll_y)),
                Rule::Thumb {
                    area,
                    content_height,
                } => {
                    let at = |scroll| {
                        scrollbar_thumb(area, content_height, scroll).map_or(0.0, |thumb| thumb.y)
                    };
                    Vec2::new(0.0, f64::from(at(0.0) - at(scroll_y)))
                }
                Rule::Sticky { sticky, inside } => {
                    let shift = sticky_shift(sticky, scroll_y + inside, self.viewport_height);
                    Vec2::new(0.0, f64::from(scroll_y - shift))
                }
            })
            .collect()
    }
}

/// How far a sticky box has been pushed from where the flow put it.
///
/// Zero until the page has scrolled far enough to take it past its inset, then
/// however much keeps it there — and never so far that it leaves its container,
/// which is what makes a sticky heading hand over to the next one.
fn sticky_shift(sticky: Sticky, scroll_y: f32, viewport_height: f32) -> f32 {
    let own = sticky.own;
    let container = sticky.container;

    if let Some(top) = sticky.top {
        let wanted = scroll_y + top - own.y;
        let room = (container.bottom() - own.bottom()).max(0.0);
        return wanted.clamp(0.0, room);
    }
    if let Some(bottom) = sticky.bottom {
        let wanted = (scroll_y + viewport_height - bottom) - own.bottom();
        let room = (container.y - own.y).min(0.0);
        return wanted.clamp(room, 0.0);
    }
    0.0
}
