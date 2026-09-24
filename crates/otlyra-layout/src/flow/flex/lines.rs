//! The arithmetic of flex layout: where the lines break, how the room along a
//! line is shared out, and how the room across the container is shared between
//! its lines.
//!
//! Pure functions over plain numbers, each named for the step of CSS Flexbox §9
//! it is, so each can be checked on its own — without a box tree, a font or a
//! layout — against what the specification says it does.

use std::ops::Range;

use otlyra_css::{AlignContent, JustifyContent};

use crate::flow::sizing::Limits;

/// How long `sizes` come to laid end to end with `gap` between each two.
pub(super) fn end_to_end(sizes: &[f32], gap: f32) -> f32 {
    sizes.iter().sum::<f32>() + gap * sizes.len().saturating_sub(1) as f32
}

/// Where a wrapping container's items break into lines (CSS Flexbox §9.3,
/// step 5): as many to a line as fit, and never fewer than one, so an item
/// bigger than the line has a line of its own rather than none.
///
/// `outer` is what each item takes from its line — its outer hypothetical main
/// size, which is its base size held between its limits, margins included —
/// in the order the items are laid out; `gap` goes between two items on one
/// line; and `room` is the container's inner main size, infinite for a column
/// as long as its items — which is one line, however long.
///
/// The hypothetical size rather than the base size is what makes `flex: 1 1 0;
/// min-width: 200px` wrap: every such item starts at nothing, and every one of
/// them takes two hundred pixels of its line.
pub(super) fn break_lines(outer: &[f32], gap: f32, room: f32) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut used = 0.0;
    for (index, &size) in outer.iter().enumerate() {
        let with_gap = if index == start { size } else { size + gap };
        if index > start && used + with_gap > room {
            lines.push(start..index);
            start = index;
            used = size;
        } else {
            used += with_gap;
        }
    }
    lines.push(start..outer.len());
    lines
}

/// What resolving a line's flexible lengths needs of one of its items (CSS
/// Flexbox §9.7), all of it along the main axis and every size a border box.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Flexible {
    /// Its flex base size (§9.2, step 3), which its own minimum and maximum
    /// have no say in.
    pub(super) base: f32,
    /// Its used minimum and maximum main sizes: what `min-width` and
    /// `max-width`, or `min-height` and `max-height`, came to, with `auto` as
    /// the automatic minimum (§4.5).
    pub(super) limits: Limits,
    /// Its padding and border. The base size less this is the inner base size
    /// shrinking is weighed by, and the item is never shrunk past it: its
    /// content box stops at nothing.
    pub(super) frame: f32,
    /// Its margins, which take room on the line and are not shared.
    pub(super) margins: f32,
    /// `flex-grow`.
    pub(super) grow: f32,
    /// `flex-shrink`.
    pub(super) shrink: f32,
}

impl Flexible {
    /// A size held between the item's limits, with its content box at no less
    /// than nothing (§9.7, step 5d).
    fn clamp(&self, size: f32) -> f32 {
        self.limits.clamp(size).max(self.frame)
    }

    /// Its hypothetical main size: its base size held between its limits
    /// (§9.2, step 3).
    pub(super) fn hypothetical(&self) -> f32 {
        self.clamp(self.base)
    }

    /// What it takes from its line before anything is shared out: its
    /// hypothetical main size and its margins.
    pub(super) fn outer_hypothetical(&self) -> f32 {
        self.hypothetical() + self.margins
    }

    /// Its flex factor for the way its line flexes.
    fn factor(&self, flex: Flex) -> f32 {
        match flex {
            Flex::Grow => self.grow,
            Flex::Shrink => self.shrink,
        }
    }

    /// Whether it has no part in the sharing out and is frozen at its
    /// hypothetical main size before it starts (§9.7, step 3): its factor is
    /// zero, or its limits have already moved it the way the line is going —
    /// held down by its maximum on a line that grows, or up by its minimum on
    /// one that shrinks.
    fn is_inflexible(&self, flex: Flex) -> bool {
        let hypothetical = self.hypothetical();
        self.factor(flex) == 0.0
            || match flex {
                Flex::Grow => self.base > hypothetical,
                Flex::Shrink => self.base < hypothetical,
            }
    }

    /// What its share of the free space is in proportion to (§9.7, step 5c):
    /// `flex-grow` on a line that grows, and on one that shrinks its scaled
    /// flex shrink factor — `flex-shrink` times its inner base size, so that a
    /// wide item gives up more than a narrow one with the same factor, and an
    /// item that starts at nothing gives up nothing.
    fn weight(&self, flex: Flex) -> f32 {
        match flex {
            Flex::Grow => self.grow,
            Flex::Shrink => self.shrink * (self.base - self.frame).max(0.0),
        }
    }
}

/// Which way a line's items flex (CSS Flexbox §9.7, step 1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Flex {
    /// Into the room the line has left over, by `flex-grow`.
    Grow,
    /// Out of the room the line is missing, by the scaled `flex-shrink`.
    Shrink,
}

/// The main sizes of one line's items, as border boxes: their flexible lengths
/// resolved into `room`, the line's inner main size less the gaps between its
/// items (CSS Flexbox §9.7).
///
/// Step by step as the specification has it. The line grows when its items'
/// outer hypothetical sizes leave room over, and shrinks otherwise; an item
/// with no part in that is frozen at its hypothetical size; and then, until
/// every item is frozen, what the line has left over — or is missing — is
/// shared out between the rest, each result held between the item's limits,
/// and the items that overshot are frozen where their limits held them. Which
/// ones is decided by the sum of the overshoots: a positive sum freezes the
/// items held up by their minimums, a negative one those held down by their
/// maximums, and nothing at all freezes everything. The space an item frozen
/// at its limit could not take — or could not give up — goes round again to
/// the others.
///
/// Factors that add up to less than one share out only that fraction of the
/// room there was to begin with: a lone item at `flex-grow: .5` takes half the
/// free space, not all of it.
///
/// With no room to fit into — a column as long as its items — there is no free
/// space to share, and every item is its hypothetical size.
pub(super) fn resolve_flexible_lengths(items: &[Flexible], room: f32) -> Vec<f32> {
    if !room.is_finite() {
        return items.iter().map(Flexible::hypothetical).collect();
    }
    let wanted: f32 = items.iter().map(Flexible::outer_hypothetical).sum();
    let flex = if wanted < room {
        Flex::Grow
    } else {
        Flex::Shrink
    };

    let mut targets: Vec<Target> = items
        .iter()
        .map(|item| {
            if item.is_inflexible(flex) {
                Target {
                    size: item.hypothetical(),
                    frozen: true,
                }
            } else {
                Target {
                    size: item.base,
                    frozen: false,
                }
            }
        })
        .collect();
    let initial = free_space(items, &targets, room);

    loop {
        let flexible: Vec<usize> = (0..items.len())
            .filter(|&index| !targets[index].frozen)
            .collect();
        if flexible.is_empty() {
            return targets.into_iter().map(|target| target.size).collect();
        }

        // Step 5b: what is left to share, but no more of what there was to
        // begin with than the factors add up to, when that is less than one.
        let factors: f32 = flexible
            .iter()
            .map(|&index| items[index].factor(flex))
            .sum();
        let remaining = free_space(items, &targets, room);
        let remaining = if factors < 1.0 && (initial * factors).abs() < remaining.abs() {
            initial * factors
        } else {
            remaining
        };

        // Step 5c: shared out by weight. A shrinking line takes the space from
        // its items, whichever way the sum came out.
        let remaining = match flex {
            Flex::Grow => remaining,
            Flex::Shrink => -remaining.abs(),
        };
        let weights: f32 = flexible
            .iter()
            .map(|&index| items[index].weight(flex))
            .sum();
        for &index in &flexible {
            let item = &items[index];
            let share = if weights > 0.0 {
                remaining * item.weight(flex) / weights
            } else {
                0.0
            };
            targets[index].size = item.base + share;
        }

        // Step 5d: each held between its limits, and by how much.
        let adjustments: Vec<f32> = flexible
            .iter()
            .map(|&index| {
                let target = &mut targets[index];
                let held = items[index].clamp(target.size);
                let adjustment = held - target.size;
                target.size = held;
                adjustment
            })
            .collect();

        // Step 5e: freeze the items the adjustments, added up, say overshot.
        let violation: f32 = adjustments.iter().sum();
        for (&index, adjustment) in flexible.iter().zip(adjustments) {
            targets[index].frozen = is_overflexed(violation, adjustment);
        }
    }
}

/// One item's target main size while its line is resolved (CSS Flexbox §9.7,
/// step 2), and whether it is frozen there.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Target {
    size: f32,
    frozen: bool,
}

/// What the line has left over, or is missing, with its frozen items at their
/// target main sizes and the rest at their base sizes (CSS Flexbox §9.7,
/// step 4).
fn free_space(items: &[Flexible], targets: &[Target], room: f32) -> f32 {
    let taken: f32 = items
        .iter()
        .zip(targets)
        .map(|(item, target)| {
            let size = if target.frozen {
                target.size
            } else {
                item.base
            };
            size + item.margins
        })
        .sum();
    room - taken
}

/// Whether an item its limits moved by `adjustment` is frozen, when the
/// adjustments on its line add up to `violation` (CSS Flexbox §9.7, step 5e):
/// the items held up by their minimums when the sum is positive, those held
/// down by their maximums when it is negative, and every item when it is
/// nothing.
fn is_overflexed(violation: f32, adjustment: f32) -> bool {
    if violation > 0.0 {
        adjustment > 0.0
    } else if violation < 0.0 {
        adjustment < 0.0
    } else {
        true
    }
}

/// Where the room a run of boxes leaves over goes: how much before the first,
/// and how much between each two.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(super) struct Spacing {
    pub(super) leading: f32,
    pub(super) between: f32,
}

/// The positions `justify-content` and `align-content` have in common: the
/// run of boxes is moved to one end or the middle, or the room is spread
/// between them (CSS Box Alignment 3 §4.1, §4.3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Spread {
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// How `leftover` is spread over a run of `count` boxes.
///
/// A distributed position with nothing to distribute between falls back to the
/// one Box Alignment names for it: `space-between` with one box is `start`, and
/// `space-around` and `space-evenly` with one box centre it, which their own
/// arithmetic already does.
fn spread(spread: Spread, leftover: f32, count: usize) -> Spacing {
    let boxes = count.max(1) as f32;
    let (leading, between) = match spread {
        Spread::Start => (0.0, 0.0),
        Spread::End => (leftover, 0.0),
        Spread::Center => (leftover / 2.0, 0.0),
        Spread::SpaceBetween if count > 1 => (0.0, leftover / (boxes - 1.0)),
        Spread::SpaceBetween => (0.0, 0.0),
        Spread::SpaceAround => (leftover / (boxes * 2.0), leftover / boxes),
        Spread::SpaceEvenly => (leftover / (boxes + 1.0), leftover / (boxes + 1.0)),
    };
    Spacing { leading, between }
}

/// Where `justify-content` puts a line's items along it, once their auto
/// margins have taken what they are owed (CSS Flexbox §9.5, step 12).
pub(super) fn justify(justify: JustifyContent, leftover: f32, count: usize) -> Spacing {
    let position = match justify {
        JustifyContent::Start => Spread::Start,
        JustifyContent::End => Spread::End,
        JustifyContent::Center => Spread::Center,
        JustifyContent::SpaceBetween => Spread::SpaceBetween,
        JustifyContent::SpaceAround => Spread::SpaceAround,
        JustifyContent::SpaceEvenly => Spread::SpaceEvenly,
    };
    spread(position, leftover, count)
}

/// How `align-content` shares what a multi-line container has left across it
/// between `count` lines: where they go, and how much each one grows by.
///
/// Only `stretch` — the initial value, and the reason a wrapped container's lines
/// fill it — grows the lines (CSS Flexbox §9.4, step 9); the rest move them and
/// leave them the size they are (§9.6, step 16).
pub(super) fn share_across(align: AlignContent, leftover: f32, count: usize) -> (Spacing, f32) {
    let position = match align {
        AlignContent::Stretch => return (Spacing::default(), leftover / count.max(1) as f32),
        AlignContent::Start => Spread::Start,
        AlignContent::End => Spread::End,
        AlignContent::Center => Spread::Center,
        AlignContent::SpaceBetween => Spread::SpaceBetween,
        AlignContent::SpaceAround => Spread::SpaceAround,
        AlignContent::SpaceEvenly => Spread::SpaceEvenly,
    };
    (spread(position, leftover, count), 0.0)
}

/// A container's flex lines across it: how big each is, and where they go.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LinesAcross {
    /// The container's inner cross size (CSS Flexbox §9.4, step 15).
    pub(super) container: f32,
    /// The cross size of each line, in order.
    pub(super) sizes: Vec<f32>,
    /// Before the first line, and between each two, on top of the gap.
    pub(super) spacing: Spacing,
}

/// The cross sizes of a container's lines, and where they go (CSS Flexbox §9.4
/// steps 8, 9 and 15, §9.6 step 16).
///
/// `wanted` is each line's largest outer hypothetical cross size — what its
/// items ask of it once each is as big across as its main size makes it — and
/// `gap` goes between two lines. `container` is the container's inner cross
/// size once its lines come to so much: its own where it has a definite one,
/// and otherwise what they come to held between its minimum and its maximum.
///
/// The one line of a single-line container is the container's cross size,
/// whatever its items want: an item bigger than a definite size overflows the
/// line rather than growing it, which is how a sidebar in a row as tall as the
/// window is as tall as the window, and scrolls; and a line smaller than the
/// container's minimum grows to it (step 8), which is what an item centred in
/// a `min-height` hero is centred in. A multi-line container shares out what
/// its lines leave by `align-content`, which a single-line one does not have
/// to ask — a leftover there is only where the container has a definite size
/// or its minimum held it up.
pub(super) fn size_lines(
    wanted: &[f32],
    container: impl FnOnce(f32) -> f32,
    single_line: bool,
    gap: f32,
    align: AlignContent,
) -> LinesAcross {
    let content = end_to_end(wanted, gap);
    let cross = container(content);
    if single_line {
        return LinesAcross {
            container: cross,
            sizes: wanted.iter().map(|_| cross).collect(),
            spacing: Spacing::default(),
        };
    }
    let (spacing, grow) = share_across(align, (cross - content).max(0.0), wanted.len());
    LinesAcross {
        container: cross,
        sizes: wanted.iter().map(|size| size + grow).collect(),
        spacing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_fill_a_line_before_the_next_one_starts() {
        assert_eq!(
            break_lines(&[100.0, 100.0, 100.0, 100.0], 0.0, 300.0),
            vec![0..3, 3..4]
        );
        // Exactly full is full, not over.
        assert_eq!(break_lines(&[150.0, 150.0], 0.0, 300.0), vec![0..2]);
    }

    #[test]
    fn a_gap_takes_room_on_the_line_it_is_on() {
        assert_eq!(
            break_lines(&[100.0, 100.0, 100.0], 10.0, 300.0),
            vec![0..2, 2..3]
        );
    }

    #[test]
    fn an_item_bigger_than_the_line_has_a_line_of_its_own() {
        assert_eq!(
            break_lines(&[50.0, 500.0, 50.0], 0.0, 300.0),
            vec![0..1, 1..2, 2..3]
        );
    }

    #[test]
    fn with_no_room_to_fit_into_there_is_one_line() {
        assert_eq!(
            break_lines(&[1e6, 1e6, 1e6], 10.0, f32::INFINITY),
            vec![0..3]
        );
    }

    /// An item with no limits, no frame and no margins.
    fn flexible(base: f32, grow: f32, shrink: f32) -> Flexible {
        Flexible {
            base,
            limits: Limits {
                min: 0.0,
                max: f32::INFINITY,
            },
            frame: 0.0,
            margins: 0.0,
            grow,
            shrink,
        }
    }

    fn limited(item: Flexible, min: f32, max: f32) -> Flexible {
        Flexible {
            limits: Limits { min, max },
            ..item
        }
    }

    #[test]
    fn free_space_goes_by_the_grow_factors() {
        let items = [flexible(100.0, 1.0, 1.0), flexible(100.0, 3.0, 1.0)];
        assert_eq!(resolve_flexible_lengths(&items, 600.0), vec![200.0, 400.0]);
    }

    /// Shrinking is weighed by the base sizes as well as the factors: the
    /// wider item gives up three times as much.
    #[test]
    fn missing_space_is_taken_in_proportion_to_the_base_sizes() {
        let items = [flexible(300.0, 0.0, 1.0), flexible(100.0, 0.0, 1.0)];
        assert_eq!(resolve_flexible_lengths(&items, 200.0), vec![150.0, 50.0]);
    }

    /// Weighed by the inner base size: padding and border are not given up,
    /// and a content box stops at nothing.
    #[test]
    fn shrinking_spares_the_frame() {
        let framed = Flexible {
            frame: 100.0,
            ..flexible(200.0, 0.0, 1.0)
        };
        let items = [framed, flexible(100.0, 0.0, 1.0)];
        assert_eq!(resolve_flexible_lengths(&items, 250.0), vec![175.0, 75.0]);
        assert_eq!(resolve_flexible_lengths(&items, 0.0), vec![100.0, 0.0]);
    }

    /// Two `flex: 1` items, the first at `max-width: 100px`: it stops there,
    /// and what it could not take goes to the other.
    #[test]
    fn an_item_held_at_its_maximum_hands_the_rest_on() {
        let items = [
            limited(flexible(0.0, 1.0, 1.0), 0.0, 100.0),
            flexible(0.0, 1.0, 1.0),
        ];
        assert_eq!(resolve_flexible_lengths(&items, 400.0), vec![100.0, 300.0]);
    }

    /// An item its minimum holds up is frozen there, and the others give up
    /// what it could not.
    #[test]
    fn an_item_held_at_its_minimum_hands_the_rest_on() {
        let items = [
            limited(flexible(100.0, 0.0, 1.0), 90.0, f32::INFINITY),
            flexible(100.0, 0.0, 1.0),
        ];
        assert_eq!(resolve_flexible_lengths(&items, 100.0), vec![90.0, 10.0]);

        // Growing too: the item with the minimum is frozen at it, and the
        // other takes what is left.
        let items = [
            limited(flexible(0.0, 1.0, 1.0), 150.0, f32::INFINITY),
            flexible(0.0, 1.0, 1.0),
        ];
        assert_eq!(resolve_flexible_lengths(&items, 200.0), vec![150.0, 50.0]);

        // And where its minimum is more than the line, it is frozen at it
        // from the start, and the other item has nothing to give.
        assert_eq!(resolve_flexible_lengths(&items, 100.0), vec![150.0, 0.0]);
    }

    /// Factors that add up to less than one share out that fraction of the
    /// free space, growing and shrinking alike.
    #[test]
    fn factors_below_one_share_out_a_fraction() {
        assert_eq!(
            resolve_flexible_lengths(&[flexible(0.0, 0.5, 1.0)], 400.0),
            vec![200.0]
        );
        let items = [flexible(100.0, 0.0, 0.25), flexible(100.0, 0.0, 0.25)];
        assert_eq!(resolve_flexible_lengths(&items, 100.0), vec![75.0, 75.0]);
    }

    /// An item whose limits already moved it the way the line goes has no part
    /// in the sharing out: it is its hypothetical size.
    #[test]
    fn an_item_its_limits_already_moved_is_frozen_at_them() {
        let items = [
            limited(flexible(300.0, 1.0, 1.0), 0.0, 100.0),
            flexible(100.0, 1.0, 1.0),
        ];
        assert_eq!(resolve_flexible_lengths(&items, 400.0), vec![100.0, 300.0]);
    }

    /// Margins take room and are not shared out.
    #[test]
    fn margins_take_room_from_the_line() {
        let items = [
            Flexible {
                margins: 20.0,
                ..flexible(0.0, 1.0, 1.0)
            },
            flexible(0.0, 1.0, 1.0),
        ];
        assert_eq!(resolve_flexible_lengths(&items, 220.0), vec![100.0, 100.0]);
    }

    /// With no room to fit into, every item is its base size held between its
    /// limits.
    #[test]
    fn with_no_room_every_item_is_its_hypothetical_size() {
        let items = [
            limited(flexible(0.0, 1.0, 1.0), 40.0, f32::INFINITY),
            limited(flexible(500.0, 1.0, 1.0), 0.0, 60.0),
        ];
        assert_eq!(
            resolve_flexible_lengths(&items, f32::INFINITY),
            vec![40.0, 60.0]
        );
    }

    #[test]
    fn lines_break_on_hypothetical_sizes() {
        let items = [limited(flexible(0.0, 1.0, 1.0), 200.0, f32::INFINITY); 3];
        let outer: Vec<f32> = items.iter().map(Flexible::outer_hypothetical).collect();
        assert_eq!(break_lines(&outer, 0.0, 500.0), vec![0..2, 2..3]);
    }

    /// A container with a definite cross size: that, whatever its lines come
    /// to.
    fn definite(size: f32) -> impl FnOnce(f32) -> f32 {
        move |_| size
    }

    /// A container with no cross size of its own, held between `min` and
    /// `max`.
    fn held(min: f32, max: f32) -> impl FnOnce(f32) -> f32 {
        move |lines| Limits { min, max }.clamp(lines)
    }

    #[test]
    fn stretch_grows_every_line_by_the_same_amount() {
        let across = size_lines(
            &[20.0, 40.0],
            definite(100.0),
            false,
            0.0,
            AlignContent::Stretch,
        );
        assert_eq!(across.sizes, vec![40.0, 60.0]);
        assert_eq!(across.spacing, Spacing::default());
    }

    #[test]
    fn space_between_puts_the_last_line_at_the_far_edge() {
        let across = size_lines(
            &[54.0, 54.0],
            definite(200.0),
            false,
            0.0,
            AlignContent::SpaceBetween,
        );
        assert_eq!(across.sizes, vec![54.0, 54.0]);
        assert_eq!(
            across.spacing,
            Spacing {
                leading: 0.0,
                between: 92.0
            }
        );
    }

    #[test]
    fn distributed_alignment_with_one_line_falls_back() {
        let one = |align| share_across(align, 60.0, 1).0;
        assert_eq!(one(AlignContent::SpaceBetween).leading, 0.0);
        assert_eq!(one(AlignContent::SpaceAround).leading, 30.0);
        assert_eq!(one(AlignContent::SpaceEvenly).leading, 30.0);
    }

    #[test]
    fn the_gaps_between_lines_are_not_shared_out() {
        let across = size_lines(
            &[20.0, 20.0],
            definite(100.0),
            false,
            10.0,
            AlignContent::End,
        );
        assert_eq!(across.spacing.leading, 50.0);
    }

    #[test]
    fn a_single_line_is_as_big_as_a_container_with_a_size() {
        let across = size_lines(&[500.0], definite(100.0), true, 0.0, AlignContent::Center);
        assert_eq!(across.sizes, vec![100.0]);
        assert_eq!(across.spacing, Spacing::default());
    }

    #[test]
    fn a_container_with_no_size_across_is_as_big_as_its_lines() {
        let across = size_lines(
            &[20.0, 30.0],
            held(0.0, f32::INFINITY),
            false,
            5.0,
            AlignContent::Stretch,
        );
        assert_eq!(across.container, 55.0);
        assert_eq!(across.sizes, vec![20.0, 30.0]);
        assert_eq!(across.spacing, Spacing::default());
    }

    /// Step 8: a single line is held between the container's minimum and
    /// maximum across, both ways.
    #[test]
    fn a_single_line_is_held_between_the_container_limits() {
        let up = size_lines(
            &[40.0],
            held(300.0, f32::INFINITY),
            true,
            0.0,
            AlignContent::Start,
        );
        assert_eq!((up.container, up.sizes), (300.0, vec![300.0]));
        let down = size_lines(&[100.0], held(0.0, 50.0), true, 0.0, AlignContent::Start);
        assert_eq!((down.container, down.sizes), (50.0, vec![50.0]));
    }

    /// Steps 15 and 16: lines held up by the container's minimum have the
    /// rest to share by `align-content`, as they would in a definite size.
    #[test]
    fn a_minimum_across_leaves_room_for_align_content() {
        let centred = size_lines(
            &[20.0, 20.0],
            held(400.0, f32::INFINITY),
            false,
            0.0,
            AlignContent::Center,
        );
        assert_eq!(centred.container, 400.0);
        assert_eq!(centred.sizes, vec![20.0, 20.0]);
        assert_eq!(centred.spacing.leading, 180.0);

        let stretched = size_lines(
            &[20.0, 40.0],
            held(400.0, f32::INFINITY),
            false,
            0.0,
            AlignContent::Stretch,
        );
        assert_eq!(stretched.sizes, vec![190.0, 210.0]);
    }
}
