//! Track sizing (CSS Grid 2 §12): how wide each column is, and how tall each
//! row, from the sizing functions the page gave them and what the items in
//! them contribute.
//!
//! Pure arithmetic over numbers the container has already measured, the same
//! for both axes. Its steps are the specification's, in its order: initialize
//! (§12.4), resolve intrinsic sizes (§12.5), maximize (§12.6), expand the
//! flexible tracks (§12.7) and stretch the `auto` ones (§12.8).
//!
//! Baselines are not modelled, so §12.5 step 1, which shims items aligned on
//! one, is not taken. The redo of §12.6 against a container's maximum is not
//! taken either; the one of §12.7 is.

use std::ops::Range;

use otlyra_css::{TrackMax, TrackMin, TrackSize};

use crate::flow::sizing::Limits;

/// A track's minimum sizing function, resolved.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum Minimum {
    Fixed(f32),
    Auto,
    MinContent,
    MaxContent,
}

/// A track's maximum sizing function, resolved.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum Maximum {
    Fixed(f32),
    Flex(f32),
    Auto,
    MinContent,
    MaxContent,
    FitContent(f32),
}

/// One track's sizing functions, with their lengths resolved.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Track {
    pub(super) min: Minimum,
    pub(super) max: Maximum,
}

impl Track {
    /// A computed track size against the grid's size along the axis, when it
    /// has one. Where it has not, a percentage makes the whole function `auto`
    /// (§7.2.1).
    pub(super) fn resolve(size: &TrackSize, basis: Option<f32>) -> Self {
        let length = |length: &otlyra_css::Length| length.definite(basis);
        let min = match &size.min {
            TrackMin::Length(value) => length(value).map_or(Minimum::Auto, Minimum::Fixed),
            TrackMin::Auto => Minimum::Auto,
            TrackMin::MinContent => Minimum::MinContent,
            TrackMin::MaxContent => Minimum::MaxContent,
        };
        let max = match &size.max {
            TrackMax::Length(value) => length(value).map_or(Maximum::Auto, Maximum::Fixed),
            TrackMax::Fr(fr) => Maximum::Flex(fr.max(0.0)),
            TrackMax::Auto => Maximum::Auto,
            TrackMax::MinContent => Maximum::MinContent,
            TrackMax::MaxContent => Maximum::MaxContent,
            TrackMax::FitContent(value) => {
                length(value).map_or(Maximum::MaxContent, Maximum::FitContent)
            }
        };
        Self { min, max }
    }

    fn flex(self) -> Option<f32> {
        match self.max {
            Maximum::Flex(fr) => Some(fr),
            Maximum::Fixed(_)
            | Maximum::Auto
            | Maximum::MinContent
            | Maximum::MaxContent
            | Maximum::FitContent(_) => None,
        }
    }

    fn intrinsic_min(self) -> bool {
        match self.min {
            Minimum::Auto | Minimum::MinContent | Minimum::MaxContent => true,
            Minimum::Fixed(_) => false,
        }
    }

    fn intrinsic_max(self) -> bool {
        match self.max {
            Maximum::Auto | Maximum::MinContent | Maximum::MaxContent | Maximum::FitContent(_) => {
                true
            }
            Maximum::Fixed(_) | Maximum::Flex(_) => false,
        }
    }

    fn max_content_max(self) -> bool {
        match self.max {
            Maximum::Auto | Maximum::MaxContent | Maximum::FitContent(_) => true,
            Maximum::Fixed(_) | Maximum::Flex(_) | Maximum::MinContent => false,
        }
    }
}

/// What one item asks of the tracks it spans, as outer sizes — margins in.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Contribution {
    pub(super) tracks: Range<usize>,
    /// Its minimum contribution: the automatic minimum, or its own minimum
    /// size (§6.6).
    pub(super) minimum: f32,
    pub(super) min_content: f32,
    pub(super) max_content: f32,
}

/// The room the tracks are sized in.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum Space {
    /// A size to fill.
    Definite(f32),
    /// None: the grid is being made as small as it can be.
    MinContent,
    /// None: the grid is as big as its content, which is also how a grid of
    /// no definite height sizes its rows.
    MaxContent,
}

/// The size of each track (§12.3).
///
/// `container` is the grid's own minimum and maximum along the axis, which
/// hold flexible tracks sized in an indefinite space (§12.7) and give `auto`
/// tracks something to stretch into (§12.8). `stretch_auto` is whether the
/// content distribution is `normal` or `stretch`, which is what lets them.
pub(super) fn size_tracks(
    tracks: &[Track],
    items: &[Contribution],
    space: Space,
    container: Limits,
    gap: f32,
    stretch_auto: bool,
) -> Vec<f32> {
    let gaps = gap * tracks.len().saturating_sub(1) as f32;

    // §12.4: every track starts at its fixed minimum and grows no further than
    // its fixed maximum; anything intrinsic starts at nothing and has no limit
    // yet.
    let mut base: Vec<f32> = tracks
        .iter()
        .map(|track| match track.min {
            Minimum::Fixed(size) => size,
            Minimum::Auto | Minimum::MinContent | Minimum::MaxContent => 0.0,
        })
        .collect();
    let mut limit: Vec<f32> = tracks
        .iter()
        .zip(&base)
        .map(|(track, &base)| match track.max {
            Maximum::Fixed(size) => size.max(base),
            Maximum::Flex(_)
            | Maximum::Auto
            | Maximum::MinContent
            | Maximum::MaxContent
            | Maximum::FitContent(_) => f32::INFINITY,
        })
        .collect();

    resolve_intrinsic(tracks, items, space, gap, &mut base, &mut limit);

    // §12.6: share out what the space has left, equally, until each track
    // reaches its limit. Under a max-content constraint that space is
    // infinite, and every track goes to its limit.
    match space {
        Space::Definite(size) => {
            let free = size - base.iter().sum::<f32>() - gaps;
            grow_equally(&mut base, &limit, free, |_| true);
        }
        Space::MaxContent => base.clone_from(&limit),
        Space::MinContent => {}
    }

    expand_flexible(tracks, items, space, container, gap, &mut base);

    // §12.8: what is still left goes to the `auto` tracks, equally.
    if stretch_auto {
        let room = match space {
            Space::Definite(size) => Some(size),
            Space::MinContent | Space::MaxContent => (container.min > 0.0).then_some(container.min),
        };
        if let Some(room) = room {
            let free = room - base.iter().sum::<f32>() - gaps;
            let autos = tracks
                .iter()
                .filter(|track| track.max == Maximum::Auto)
                .count();
            if free > 0.0 && autos > 0 {
                let share = free / autos as f32;
                for (size, track) in base.iter_mut().zip(tracks) {
                    if track.max == Maximum::Auto {
                        *size += share;
                    }
                }
            }
        }
    }
    base
}

/// §12.5: the intrinsic tracks' base sizes and limits, from the items in them.
fn resolve_intrinsic(
    tracks: &[Track],
    items: &[Contribution],
    space: Space,
    gap: f32,
    base: &mut [f32],
    limit: &mut [f32],
) {
    let crosses_flex = |item: &Contribution| {
        tracks[item.tracks.clone()]
            .iter()
            .any(|t| t.flex().is_some())
    };
    // The contribution an `auto` minimum takes: the minimum contribution, or
    // under a constraint the contribution that constraint asks for.
    let auto_min = |item: &Contribution| match space {
        Space::Definite(_) => item.minimum,
        Space::MinContent => item.min_content,
        Space::MaxContent => item.max_content,
    };

    // Step 2: items in one track that is not flexible.
    for (index, track) in tracks.iter().enumerate() {
        if track.flex().is_some() {
            continue;
        }
        let single: Vec<&Contribution> = items
            .iter()
            .filter(|item| item.tracks == (index..index + 1))
            .collect();
        if single.is_empty() {
            continue;
        }
        let most = |of: &dyn Fn(&Contribution) -> f32| {
            single.iter().map(|item| of(item)).fold(0.0f32, f32::max)
        };
        let limited = |value: f32, item: &Contribution| match track.max {
            Maximum::Fixed(cap) => value.min(cap).max(item.minimum),
            Maximum::Flex(_)
            | Maximum::Auto
            | Maximum::MinContent
            | Maximum::MaxContent
            | Maximum::FitContent(_) => value,
        };
        base[index] = base[index].max(match track.min {
            Minimum::Fixed(size) => size,
            Minimum::Auto => most(&|item| limited(auto_min(item), item)),
            Minimum::MinContent => most(&|item| item.min_content),
            Minimum::MaxContent => most(&|item| item.max_content),
        });
        limit[index] = match track.max {
            Maximum::Fixed(size) => size,
            Maximum::MinContent => most(&|item| item.min_content),
            Maximum::Auto | Maximum::MaxContent => most(&|item| item.max_content),
            Maximum::FitContent(cap) => most(&|item| item.max_content).min(cap.max(base[index])),
            Maximum::Flex(_) => limit[index],
        };
        if limit[index] < base[index] {
            limit[index] = base[index];
        }
    }

    // Step 3: items across several tracks, none of them flexible, the fewest
    // tracks first.
    let mut spanning: Vec<&Contribution> = items
        .iter()
        .filter(|item| item.tracks.len() > 1 && !crosses_flex(item))
        .collect();
    spanning.sort_by_key(|item| item.tracks.len());
    for group in spanning.chunk_by(|a, b| a.tracks.len() == b.tracks.len()) {
        let phases = [
            // Intrinsic minimums, content-based minimums, max-content
            // minimums, intrinsic maximums, max-content maximums.
            Phase {
                target: Target::Base,
                affected: &|t: Track| t.intrinsic_min(),
                contribution: &|item| auto_min(item),
            },
            Phase {
                target: Target::Base,
                affected: &|t: Track| matches!(t.min, Minimum::MinContent | Minimum::MaxContent),
                contribution: &|item| item.min_content,
            },
            Phase {
                target: Target::Base,
                affected: &|t: Track| {
                    t.min == Minimum::MaxContent
                        || (t.min == Minimum::Auto && space == Space::MaxContent)
                },
                contribution: &|item| item.max_content,
            },
            Phase {
                target: Target::Limit,
                affected: &|t: Track| t.intrinsic_max(),
                contribution: &|item| item.min_content,
            },
            Phase {
                target: Target::Limit,
                affected: &|t: Track| t.max_content_max(),
                contribution: &|item| item.max_content,
            },
        ];
        for phase in &phases {
            distribute(tracks, group, phase, gap, base, limit);
            for (base, limit) in base.iter().zip(limit.iter_mut()) {
                if *limit < *base {
                    *limit = *base;
                }
            }
        }
    }

    // Step 4: items that cross a flexible track give their minimum to the
    // flexible tracks they span, by flex factor.
    for item in items.iter().filter(|item| crosses_flex(item)) {
        let span = item.tracks.clone();
        let spent: f32 = base[span.clone()].iter().sum::<f32>() + gap * (span.len() - 1) as f32;
        let extra = auto_min(item) - spent;
        if extra <= 0.0 {
            continue;
        }
        let factors: f32 = tracks[span.clone()].iter().filter_map(|t| t.flex()).sum();
        let flexible = tracks[span.clone()]
            .iter()
            .filter(|t| t.flex().is_some())
            .count();
        for index in span {
            if let Some(fr) = tracks[index].flex() {
                base[index] += if factors > 0.0 {
                    extra * fr / factors
                } else {
                    extra / flexible as f32
                };
            }
        }
    }
    for (base, limit) in base.iter().zip(limit.iter_mut()) {
        if *limit < *base {
            *limit = *base;
        }
    }

    // Step 5: a limit nothing gave the track is its base size.
    for (base, limit) in base.iter().zip(limit.iter_mut()) {
        if limit.is_infinite() {
            *limit = *base;
        }
    }
}

/// Which of a track's two sizes a phase of §12.5.1 grows.
#[derive(Copy, Clone, PartialEq)]
enum Target {
    Base,
    Limit,
}

/// One phase of §12.5.1: which size of which tracks grows, and by which of
/// the items' contributions.
struct Phase<'p> {
    target: Target,
    affected: &'p dyn Fn(Track) -> bool,
    contribution: &'p dyn Fn(&Contribution) -> f32,
}

/// §12.5.1: distribute what the items of one span count need beyond their
/// tracks' current sizes over the affected tracks, equally, as far as each
/// may grow — and past that into the ones that may grow further.
fn distribute(
    tracks: &[Track],
    items: &[&Contribution],
    phase: &Phase<'_>,
    gap: f32,
    base: &mut [f32],
    limit: &mut [f32],
) {
    let Phase {
        target,
        affected,
        contribution,
    } = *phase;
    let current = |index: usize, base: &[f32], limit: &[f32]| match target {
        Target::Base => base[index],
        Target::Limit if limit[index].is_infinite() => base[index],
        Target::Limit => limit[index],
    };
    let mut planned = vec![0.0f32; tracks.len()];
    for item in items {
        let span = item.tracks.clone();
        let chosen: Vec<usize> = span
            .clone()
            .filter(|&index| affected(tracks[index]))
            .collect();
        if chosen.is_empty() {
            continue;
        }
        let spent: f32 = span
            .clone()
            .map(|index| current(index, base, limit))
            .sum::<f32>()
            + gap * (span.len() - 1) as f32;
        let mut extra = contribution(item) - spent;
        if extra <= 0.0 {
            continue;
        }
        let mut increase = vec![0.0f32; tracks.len()];
        // As far as each may grow: a base size up to its limit, a limit that
        // has none yet without end, and a `fit-content` one up to its argument.
        let ceiling = |index: usize| match target {
            Target::Base => limit[index],
            Target::Limit => match tracks[index].max {
                Maximum::FitContent(cap) => cap.max(base[index]),
                Maximum::Fixed(_)
                | Maximum::Flex(_)
                | Maximum::Auto
                | Maximum::MinContent
                | Maximum::MaxContent => f32::INFINITY,
            },
        };
        extra = spread(&chosen, extra, &mut increase, |index, increase| {
            ceiling(index) - current(index, base, limit) - increase
        });
        // Past their limits, into the tracks that may take it.
        if extra > 0.0 {
            let beyond: Vec<usize> = match target {
                Target::Base => chosen
                    .iter()
                    .copied()
                    .filter(|&index| {
                        tracks[index].max_content_max() || tracks[index].intrinsic_max()
                    })
                    .collect(),
                Target::Limit => Vec::new(),
            };
            let beyond = if beyond.is_empty() {
                chosen.clone()
            } else {
                beyond
            };
            let share = extra / beyond.len() as f32;
            for index in beyond {
                increase[index] += share;
            }
        }
        for index in chosen {
            planned[index] = planned[index].max(increase[index]);
        }
    }
    for (index, increase) in planned.into_iter().enumerate() {
        if increase <= 0.0 {
            continue;
        }
        match target {
            Target::Base => base[index] += increase,
            Target::Limit => {
                let from = current(index, base, limit);
                limit[index] = from + increase;
            }
        }
    }
}

/// Share `extra` equally over `tracks`, each up to the room `room` says it
/// has left given what it has taken so far; returns what none could take.
fn spread(
    tracks: &[usize],
    mut extra: f32,
    increase: &mut [f32],
    room: impl Fn(usize, f32) -> f32,
) -> f32 {
    let mut open: Vec<usize> = tracks.to_vec();
    while extra > 1e-4 && !open.is_empty() {
        let share = extra / open.len() as f32;
        let mut next = Vec::with_capacity(open.len());
        for index in open {
            let left = room(index, increase[index]).max(0.0);
            let take = share.min(left);
            increase[index] += take;
            extra -= take;
            if take < share {
                continue;
            }
            next.push(index);
        }
        open = next;
    }
    extra.max(0.0)
}

/// §12.6: grow each track that `grows` by an equal share of `free`, freezing
/// each at its limit and sharing what it could not take among the rest.
fn grow_equally(base: &mut [f32], limit: &[f32], free: f32, grows: impl Fn(usize) -> bool) {
    if free <= 0.0 {
        return;
    }
    let tracks: Vec<usize> = (0..base.len()).filter(|&index| grows(index)).collect();
    let mut increase = vec![0.0f32; base.len()];
    spread(&tracks, free, &mut increase, |index, taken| {
        limit[index] - base[index] - taken
    });
    for (size, increase) in base.iter_mut().zip(increase) {
        *size += increase;
    }
}

/// §12.7: the flexible tracks, each its flex factor's share of the fraction
/// the space leaves, and no smaller than its base size.
fn expand_flexible(
    tracks: &[Track],
    items: &[Contribution],
    space: Space,
    container: Limits,
    gap: f32,
    base: &mut [f32],
) {
    if tracks.iter().all(|track| track.flex().is_none()) {
        return;
    }
    let gaps = gap * tracks.len().saturating_sub(1) as f32;
    let fraction = match space {
        Space::MinContent => return,
        Space::Definite(size) => fr_size(tracks, base, 0..tracks.len(), size - gaps),
        Space::MaxContent => {
            // The largest fraction any flexible track or any item that
            // crosses one asks for.
            let from_tracks = tracks
                .iter()
                .zip(base.iter())
                .filter_map(|(track, &size)| {
                    track
                        .flex()
                        .map(|fr| if fr > 1.0 { size / fr } else { size })
                })
                .fold(0.0f32, f32::max);
            let from_items = items
                .iter()
                .filter(|item| {
                    tracks[item.tracks.clone()]
                        .iter()
                        .any(|t| t.flex().is_some())
                })
                .map(|item| {
                    let span = item.tracks.clone();
                    let gaps_in = gap * span.len().saturating_sub(1) as f32;
                    fr_size(tracks, base, span, item.max_content - gaps_in)
                })
                .fold(0.0f32, f32::max);
            let fraction = from_tracks.max(from_items);
            // Held by the container's own limits: a grid whose flexible rows
            // come out taller than its maximum, or shorter than its minimum,
            // sizes them again in that height.
            let total: f32 = sizes_at(tracks, base, fraction).iter().sum::<f32>() + gaps;
            let held = container.clamp(total);
            if held == total {
                fraction
            } else {
                fr_size(tracks, base, 0..tracks.len(), held - gaps)
            }
        }
    };
    let sized = sizes_at(tracks, base, fraction);
    base.copy_from_slice(&sized);
}

/// Every track's size with the flexible ones at `fraction` per `fr`.
fn sizes_at(tracks: &[Track], base: &[f32], fraction: f32) -> Vec<f32> {
    tracks
        .iter()
        .zip(base)
        .map(|(track, &size)| track.flex().map_or(size, |fr| size.max(fraction * fr)))
        .collect()
}

/// §12.7.1: the size of one `fr` that makes the tracks in `span` fill `room`,
/// treating as inflexible any flexible track whose base size is more than its
/// share.
fn fr_size(tracks: &[Track], base: &[f32], span: Range<usize>, room: f32) -> f32 {
    let mut inflexible = vec![false; tracks.len()];
    loop {
        let mut left = room;
        let mut factors = 0.0f32;
        for index in span.clone() {
            match tracks[index].flex() {
                Some(fr) if !inflexible[index] => factors += fr,
                Some(_) | None => left -= base[index],
            }
        }
        if left <= 0.0 {
            return 0.0;
        }
        let fraction = left / factors.max(1.0);
        let mut changed = false;
        for index in span.clone() {
            if let Some(fr) = tracks[index].flex()
                && !inflexible[index]
                && fraction * fr < base[index]
            {
                inflexible[index] = true;
                changed = true;
            }
        }
        if !changed {
            return fraction;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(size: f32) -> Track {
        Track {
            min: Minimum::Fixed(size),
            max: Maximum::Fixed(size),
        }
    }

    fn fr(share: f32) -> Track {
        Track {
            min: Minimum::Auto,
            max: Maximum::Flex(share),
        }
    }

    const AUTO: Track = Track {
        min: Minimum::Auto,
        max: Maximum::Auto,
    };

    fn item(tracks: Range<usize>, min_content: f32, max_content: f32) -> Contribution {
        Contribution {
            tracks,
            minimum: min_content,
            min_content,
            max_content,
        }
    }

    fn sized(tracks: &[Track], items: &[Contribution], space: Space) -> Vec<f32> {
        size_tracks(tracks, items, space, Limits::NONE, 0.0, true)
    }

    fn close(actual: &[f32], expected: &[f32]) {
        assert_eq!(
            actual.len(),
            expected.len(),
            "{actual:?} against {expected:?}"
        );
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 0.01, "{actual:?} against {expected:?}");
        }
    }

    /// An unbreakable item in one of two `1fr` columns keeps its width; the
    /// other takes what is left (§12.7.1).
    #[test]
    fn a_flexible_track_is_no_narrower_than_what_it_holds() {
        let tracks = [fr(1.0), fr(1.0)];
        close(
            &sized(&tracks, &[item(0..1, 300.0, 300.0)], Space::Definite(400.0)),
            &[300.0, 100.0],
        );
    }

    /// `minmax(150px, 1fr)` three times in 400 overflows at 150 each.
    #[test]
    fn a_fixed_minimum_holds_against_too_little_room() {
        let track = Track {
            min: Minimum::Fixed(150.0),
            max: Maximum::Flex(1.0),
        };
        close(
            &sized(&[track; 3], &[], Space::Definite(400.0)),
            &[150.0, 150.0, 150.0],
        );
    }

    /// `auto 1fr` with a long paragraph: the `auto` track grows to its
    /// max-content only as far as the room allows (§12.6).
    #[test]
    fn an_auto_track_is_held_to_the_room() {
        let sizes = sized(
            &[AUTO, fr(1.0)],
            &[item(0..1, 80.0, 2000.0)],
            Space::Definite(400.0),
        );
        close(&sizes, &[400.0, 0.0]);
    }

    /// A span over two empty `auto` tracks splits its size between them.
    #[test]
    fn a_span_is_shared_by_the_tracks_it_crosses() {
        close(
            &sized(
                &[AUTO, AUTO],
                &[item(0..2, 300.0, 300.0)],
                Space::MaxContent,
            ),
            &[150.0, 150.0],
        );
    }

    /// A span over a fixed and an `auto` track grows only the `auto` one.
    #[test]
    fn a_span_grows_only_its_intrinsic_tracks() {
        close(
            &sized(
                &[fixed(100.0), AUTO],
                &[item(0..2, 300.0, 300.0)],
                Space::MaxContent,
            ),
            &[100.0, 200.0],
        );
    }

    /// `fit-content(100px)` holds max-content at its argument.
    #[test]
    fn fit_content_holds_the_content_at_its_argument() {
        let track = Track {
            min: Minimum::Auto,
            max: Maximum::FitContent(100.0),
        };
        close(
            &size_tracks(
                &[track],
                &[item(0..1, 50.0, 300.0)],
                Space::Definite(1000.0),
                Limits::NONE,
                0.0,
                false,
            ),
            &[100.0],
        );
    }

    /// `minmax(100px, 200px)` twice in 1000 grow to their maximum and no
    /// further.
    #[test]
    fn a_track_grows_to_its_maximum() {
        let track = Track {
            min: Minimum::Fixed(100.0),
            max: Maximum::Fixed(200.0),
        };
        close(
            &size_tracks(
                &[track; 2],
                &[],
                Space::Definite(1000.0),
                Limits::NONE,
                0.0,
                false,
            ),
            &[200.0, 200.0],
        );
    }

    /// Three `auto` tracks with small items stretch to fill (§12.8).
    #[test]
    fn auto_tracks_stretch_to_fill() {
        let items = [
            item(0..1, 10.0, 20.0),
            item(1..2, 10.0, 20.0),
            item(2..3, 10.0, 20.0),
        ];
        close(
            &sized(&[AUTO; 3], &items, Space::Definite(300.0)),
            &[100.0, 100.0, 100.0],
        );
    }

    /// `1fr 2fr` rows in 300 are 100 and 200.
    #[test]
    fn flexible_tracks_share_by_factor() {
        close(
            &sized(&[fr(1.0), fr(2.0)], &[], Space::Definite(300.0)),
            &[100.0, 200.0],
        );
    }

    /// A `minmax(0, 1fr)` row of no definite height is as tall as its item.
    #[test]
    fn an_indefinite_flexible_track_is_its_content() {
        let track = Track {
            min: Minimum::Fixed(0.0),
            max: Maximum::Flex(1.0),
        };
        close(
            &sized(&[track], &[item(0..1, 50.0, 50.0)], Space::MaxContent),
            &[50.0],
        );
    }

    /// A single `auto` row stretches to the container's minimum height.
    #[test]
    fn an_auto_track_stretches_to_the_containers_minimum() {
        let sizes = size_tracks(
            &[AUTO],
            &[item(0..1, 20.0, 20.0)],
            Space::MaxContent,
            Limits {
                min: 80.0,
                max: f32::INFINITY,
            },
            0.0,
            true,
        );
        close(&sizes, &[80.0]);
    }

    /// MDN's page grid at 1280: `minmax(0,1fr) minmax(0,240px) 32px
    /// minmax(0,736px) 32px minmax(0,240px) minmax(0,1fr)`, content and gutters
    /// at their maximum, the flexible edges taking nothing.
    #[test]
    fn the_mdn_page_grid_at_1280() {
        let capped = |size: f32| Track {
            min: Minimum::Fixed(0.0),
            max: Maximum::Fixed(size),
        };
        let edge = Track {
            min: Minimum::Fixed(0.0),
            max: Maximum::Flex(1.0),
        };
        let tracks = [
            edge,
            capped(240.0),
            fixed(32.0),
            capped(736.0),
            fixed(32.0),
            capped(240.0),
            edge,
        ];
        close(
            &sized(&tracks, &[], Space::Definite(1280.0)),
            &[0.0, 240.0, 32.0, 736.0, 32.0, 240.0, 0.0],
        );
    }
}
