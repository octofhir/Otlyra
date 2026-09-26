//! `text-overflow`: a line its block cuts off ends in a marker, and what would
//! have been under the marker is not drawn (CSS Overflow 4 §3.1).
//!
//! Done on a line's fragments once they are placed, so what is cut is what
//! the reader would have seen: text is cut between clusters, an atomic inline
//! the marker would overlap is dropped whole, and an inline box's background
//! stops where its text does. The marker is drawn, and is not text a
//! selection can take.

use std::sync::Arc;

use otlyra_css::ComputedStyle;
use otlyra_text::{ShapedRun, ShapedText};

use crate::fragment::{Fragment, FragmentKind, Rect};

/// The marker, shaped once per block in the block's own style.
pub(super) struct Marker {
    /// Its glyphs, placed against a fragment of their own.
    run: ShapedRun,
    /// How far below the marker's top its baseline is.
    baseline: f32,
    /// Its height.
    height: f32,
    /// The block's style, which it is set and painted in.
    style: Arc<ComputedStyle>,
}

impl Marker {
    /// The marker `shaped` holds: its first run, on its first line. `None`
    /// for a marker that came to no glyphs.
    pub(super) fn new(shaped: ShapedText, style: Arc<ComputedStyle>) -> Option<Self> {
        let line = *shaped.lines.first()?;
        let mut run = shaped.runs.into_iter().next()?;
        for glyph in &mut run.glyphs {
            glyph.x -= run.offset_x;
            glyph.y -= line.top;
        }
        run.offset_x = 0.0;
        Some(Self {
            run,
            baseline: line.baseline - line.top,
            height: line.height,
            style,
        })
    }

    fn width(&self) -> f32 {
        self.run.advance
    }
}

/// Cut `line` at the content box's end edge `end`, if what is on it passes
/// that edge, and end it in `marker`.
pub(super) fn elide_line(line: &mut Fragment, end: f32, marker: &Marker) {
    let reach = line
        .children
        .iter()
        .map(right_edge)
        .fold(f32::NEG_INFINITY, f32::max);
    if reach <= end + 0.01 {
        return;
    }
    let FragmentKind::Line { baseline } = line.kind else {
        return;
    };
    let limit = end - marker.width();

    line.children.retain_mut(|child| {
        if right_edge(child) <= limit {
            return true;
        }
        if child.rect.x >= limit {
            return false;
        }
        match &mut child.kind {
            FragmentKind::Text(run) => {
                let mut local = run.clone();
                local.offset_x = 0.0;
                match local.keep_before(limit - child.rect.x) {
                    Some(cut) => {
                        child.rect.width = cut.advance;
                        *run = cut;
                        true
                    }
                    None => false,
                }
            }
            // An inline box's background and border stop where the cut text
            // inside them does.
            FragmentKind::Box if child.style.display == otlyra_css::Display::Inline => {
                child.rect.width = limit - child.rect.x;
                true
            }
            // An atomic inline the marker would overlap goes whole.
            FragmentKind::Box | FragmentKind::Image(_) | FragmentKind::Line { .. } => false,
        }
    });

    let at = line
        .children
        .iter()
        .filter(|child| {
            !matches!(child.kind, FragmentKind::Box)
                || child.style.display != otlyra_css::Display::Inline
        })
        .map(right_edge)
        .fold(line.rect.x, f32::max)
        .min(limit);
    let top = line.rect.y + baseline - marker.baseline;
    line.children.push(Fragment::new(
        line.box_id,
        Rect::new(at, top, marker.width(), marker.height),
        FragmentKind::Text(marker.run.clone()),
        Arc::clone(&marker.style),
    ));
}

/// Where a fragment on a line ends: a run where its glyphs do, anything else
/// at its edge.
fn right_edge(fragment: &Fragment) -> f32 {
    match &fragment.kind {
        FragmentKind::Text(run) => fragment.rect.x + run.advance,
        FragmentKind::Box | FragmentKind::Image(_) | FragmentKind::Line { .. } => {
            fragment.rect.right()
        }
    }
}
