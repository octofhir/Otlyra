//! What happens to a paragraph's geometry once the shaper has broken it into
//! lines: tabs jump to their stops, justified lines are spread to their room,
//! and a run is cut short for an ellipsis.
//!
//! The shaper knows nothing of tab stops, and justifies a line against the
//! width it was broken to — which, where [`crate::breaking`] chose the break,
//! is the width of what is on it rather than the room it had. So these are
//! done here, on the positioned glyphs, and each keeps a line's runs, spacers
//! and width in step with one another.

use crate::engine::{LineEnd, ShapedRun, ShapedText};

/// Move what follows each tab to the next tab stop.
///
/// A tab in CSS is a jump and not a character: what it advances by is however far
/// it is to the next stop, so it can only be settled once the glyphs before it on
/// the line have been placed. The shaper knows nothing of stops — it gives the
/// tab whatever advance the font has for it — so the glyphs after one are moved
/// along here, and the line grows by what they moved.
///
/// Left to right, because where each tab lands depends on the ones before it. A
/// line that was *broken* with the tab at its font width was broken a little
/// early, which shows only where a paragraph both preserves tabs and wraps.
pub(crate) fn expand_tabs(shaped: &mut ShapedText, stop: f32) {
    if stop <= 0.0 {
        return;
    }

    for line in 0..shaped.lines.len() {
        for (run_index, glyph_index) in tabs_on(shaped, line) {
            let run = &shaped.runs[run_index];
            let at = run.glyphs[glyph_index].x;
            let after = run
                .glyphs
                .get(glyph_index + 1)
                .map_or(run.offset_x + run.advance, |glyph| glyph.x);
            // The next stop strictly past where the tab starts: a tab that lands
            // exactly on one still goes to the following one, which is what makes
            // a tab always take room.
            let target = ((at / stop).floor() + 1.0) * stop;
            let delta = target - after;
            if delta.abs() < 0.01 {
                continue;
            }
            shift_after(shaped, line, run_index, glyph_index, delta);
        }
    }
}

/// Where the tabs on one line are, as a run and a glyph in it, left to right.
///
/// Taken before anything moves and read back as things do: shifting a glyph does
/// not change which glyph it is, and each tab's own position is read again when
/// its turn comes.
///
/// Each glyph is asked which character it drew rather than counted against the
/// text: a tab is never part of a ligature, but anything ligated in front of one
/// on the same line would otherwise put the jump on the wrong glyph.
fn tabs_on(shaped: &ShapedText, line: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (run_index, run) in shaped.runs.iter().enumerate() {
        if run.line != line {
            continue;
        }
        for (glyph_index, glyph) in run.glyphs.iter().enumerate() {
            if run
                .text
                .get(glyph.text_offset as usize..)
                .is_some_and(|rest| rest.starts_with('\t'))
            {
                out.push((run_index, glyph_index));
            }
        }
    }
    out
}

/// Move everything after one glyph on a line along by `delta`.
fn shift_after(
    shaped: &mut ShapedText,
    line: usize,
    run_index: usize,
    glyph_index: usize,
    delta: f32,
) {
    let from = shaped.runs[run_index].offset_x;

    for (index, run) in shaped.runs.iter_mut().enumerate() {
        if run.line != line || index < run_index {
            continue;
        }
        if index == run_index {
            for glyph in run.glyphs.iter_mut().skip(glyph_index + 1) {
                glyph.x += delta;
            }
            run.advance += delta;
        } else {
            run.offset_x += delta;
            for glyph in &mut run.glyphs {
                glyph.x += delta;
            }
        }
    }

    for spacer in &mut shaped.spacers {
        if spacer.line == line && spacer.x >= from {
            spacer.x += delta;
        }
    }

    if let Some(metrics) = shaped.lines.get_mut(line) {
        metrics.width += delta;
        shaped.metrics.width = shaped.metrics.width.max(metrics.width);
    }
}

/// Spread each line that `justify` applies to across its room: every line
/// that wrapped, not one a forced break or the end of the paragraph ended
/// (CSS Text 3 §7.3). What is left over is shared equally among the word
/// separators on the line — the spaces, no-break ones included — that do not
/// hang at its end (§7.1). A line with none is left as it is; `inter-character`
/// justification is not done.
pub(crate) fn justify(shaped: &mut ShapedText, rooms: &[f32]) {
    for line in 0..shaped.lines.len() {
        let metrics = shaped.lines[line];
        let Some(&room) = rooms.get(line) else {
            continue;
        };
        if metrics.end != LineEnd::Wrapped || !room.is_finite() {
            continue;
        }
        let free = room - metrics.indent - (metrics.width - metrics.trailing_space);
        let gaps = separators_on(shaped, line);
        if free <= 0.0 || gaps.is_empty() {
            continue;
        }
        let share = free / gaps.len() as f32;
        // Right to left, so that moving what follows one gap does not move the
        // gaps still to come out from under their positions.
        for &(run_index, glyph_index) in gaps.iter().rev() {
            shift_after(shaped, line, run_index, glyph_index, share);
        }
    }
}

/// The word separators on one line that do not hang at its end, as a run and
/// a glyph in it, left to right.
fn separators_on(shaped: &ShapedText, line: usize) -> Vec<(usize, usize)> {
    let mut separators = Vec::new();
    let mut before_ink = Vec::new();
    for (run_index, run) in shaped.runs.iter().enumerate() {
        if run.line != line {
            continue;
        }
        for (glyph_index, glyph) in run.glyphs.iter().enumerate() {
            let first = run
                .text
                .get(glyph.text_offset as usize..)
                .and_then(|rest| rest.chars().next());
            match first {
                Some(' ' | '\u{a0}') => before_ink.push((run_index, glyph_index)),
                Some(_) => separators.append(&mut before_ink),
                None => {}
            }
        }
    }
    separators
}

impl ShapedRun {
    /// The part of the run that ends at or before `x`, on the run's own
    /// line, cut at a cluster boundary: a cluster — a letter and its marks, or
    /// a ligature — is kept or dropped whole. `None` when not even the first
    /// cluster ends by then.
    ///
    /// The glyphs of one cluster share a text offset, so the cut falls where
    /// the offset changes: every glyph of a cluster whose last glyph would
    /// cross `x` goes, and so does everything after it.
    #[must_use]
    pub fn keep_before(&self, x: f32) -> Option<ShapedRun> {
        let end_of = |index: usize| {
            self.glyphs
                .get(index + 1)
                .map_or(self.offset_x + self.advance, |next| next.x)
        };
        let mut kept = 0;
        let mut index = 0;
        while index < self.glyphs.len() {
            let offset = self.glyphs[index].text_offset;
            let mut last = index;
            while self
                .glyphs
                .get(last + 1)
                .is_some_and(|next| next.text_offset == offset)
            {
                last += 1;
            }
            if end_of(last) > x {
                break;
            }
            kept = last + 1;
            index = last + 1;
        }
        if kept == 0 {
            return None;
        }
        let end = end_of(kept - 1);
        let text_end = self
            .glyphs
            .get(kept)
            .map_or(self.text.len(), |next| next.text_offset as usize);
        let mut cut = self.clone();
        cut.glyphs.truncate(kept);
        cut.advance = end - self.offset_x;
        cut.text = self.text.get(..text_end).unwrap_or_default().into();
        cut.text_range = self.text_range.start..self.text_range.start + text_end;
        Some(cut)
    }
}
