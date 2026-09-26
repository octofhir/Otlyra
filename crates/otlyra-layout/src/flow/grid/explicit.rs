//! The explicit grid along one axis (CSS Grid 2 §7): its tracks, with an
//! automatic repetition counted and written out where it stands, and the
//! names of its lines, the ones areas imply among them.

use std::collections::HashMap;
use std::ops::Range;

use otlyra_css::{LineName, NamedArea, TrackList, TrackMax, TrackMin, TrackSize};

use crate::flow::sizing::Limits;

use super::placement::GRID_LIMIT;

/// The lines of an explicit grid that carry each name, counting from zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct LineNames(HashMap<LineName, Vec<i32>>);

impl LineNames {
    pub(super) fn add(&mut self, name: LineName, line: i32) {
        let lines = self.0.entry(name).or_default();
        if let Err(at) = lines.binary_search(&line) {
            lines.insert(at, line);
        }
    }

    /// The lines with `name`, in order.
    pub(super) fn lines(&self, name: &LineName) -> &[i32] {
        self.0.get(name).map_or(&[], Vec::as_slice)
    }
}

/// One axis of the explicit grid.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ExplicitAxis {
    /// Its tracks, the automatic repetitions written out.
    pub(super) tracks: Vec<TrackSize>,
    pub(super) names: LineNames,
    /// The tracks an automatic repetition made, which `auto-fit` collapses
    /// when nothing is placed in them.
    pub(super) repeated: Option<Range<usize>>,
    /// Whether that repetition is `auto-fit`.
    pub(super) fits: bool,
}

/// What an automatic repetition is counted against: the grid's size along the
/// axis, or failing that its maximum, or failing that its minimum
/// (§7.2.3.2).
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Room {
    pub(super) size: Option<f32>,
    pub(super) limits: Limits,
    pub(super) gap: f32,
}

impl ExplicitAxis {
    /// The axis a track list, the areas and the implicit sizes make.
    ///
    /// `areas` gives, for each named area, the range of this axis's tracks it
    /// covers; `area_tracks` is how many the areas take in all, which the
    /// explicit grid is at least (§7.1). Tracks only the areas make are sized
    /// like implicit ones, from `auto`.
    pub(super) fn new(
        list: &TrackList,
        areas: &[(&NamedArea, Range<u32>)],
        area_tracks: u32,
        auto: &[TrackSize],
        room: Room,
    ) -> Self {
        let mut tracks = Vec::with_capacity(list.tracks.len());
        let mut names = LineNames::default();
        let mut repeated = None;
        let mut fits = false;
        let mut line = 0i32;

        let name_line = |names: &mut LineNames, line: i32, on: &[LineName]| {
            for name in on {
                names.add(name.clone(), line);
            }
        };

        for index in 0..=list.tracks.len() {
            name_line(&mut names, line, list.names_on(index));
            if let Some(repeat) = list
                .auto_repeat
                .as_ref()
                .filter(|repeat| repeat.at == index)
            {
                let count = repetitions(list, &repeat.tracks, room);
                let start = tracks.len();
                for _ in 0..count {
                    // The last line of one copy is the first of the next, and
                    // carries the names of both (§7.2.3.1).
                    for (at, size) in repeat.tracks.iter().enumerate() {
                        name_line(
                            &mut names,
                            line,
                            repeat.names.get(at).map_or(&[], Vec::as_slice),
                        );
                        tracks.push(size.clone());
                        line += 1;
                    }
                    name_line(
                        &mut names,
                        line,
                        repeat
                            .names
                            .get(repeat.tracks.len())
                            .map_or(&[], Vec::as_slice),
                    );
                }
                name_line(&mut names, line, &repeat.after);
                repeated = Some(start..tracks.len());
                fits = repeat.kind == otlyra_css::RepeatKind::Fit;
            }
            if let Some(size) = list.tracks.get(index) {
                tracks.push(size.clone());
                line += 1;
            }
        }

        // The areas make the grid at least as big as they are, and every
        // area names the lines it starts and ends on (§7.3.2).
        let written = tracks.len();
        for extra in written..area_tracks as usize {
            tracks.push(implicit_track(auto, (extra - written) as isize));
        }
        for (area, span) in areas {
            names.add(area.name.suffixed("-start"), span.start as i32);
            names.add(area.name.suffixed("-end"), span.end as i32);
        }

        Self {
            tracks,
            names,
            repeated,
            fits,
        }
    }
}

/// How many times an automatic repetition goes in (§7.2.3.2): as many as fit
/// in the grid's size, or its maximum; as few as reach its minimum; and once
/// where there is none of the three. Each repeated track counts as its fixed
/// maximum, or its fixed minimum where the maximum is not fixed, and never as
/// less than a pixel.
fn repetitions(list: &TrackList, repeated: &[TrackSize], room: Room) -> usize {
    let basis = room.size;
    let fixed = |size: &TrackSize| {
        let max = match &size.max {
            TrackMax::Length(length) => length.definite(basis),
            TrackMax::Fr(_)
            | TrackMax::Auto
            | TrackMax::MinContent
            | TrackMax::MaxContent
            | TrackMax::FitContent(_) => None,
        };
        let min = match &size.min {
            TrackMin::Length(length) => length.definite(basis),
            TrackMin::Auto | TrackMin::MinContent | TrackMin::MaxContent => None,
        };
        match (max, min) {
            (Some(max), Some(min)) => Some(max.max(min)),
            (Some(size), None) | (None, Some(size)) => Some(size),
            (None, None) => None,
        }
    };
    // The tracks outside the repetition take their fixed size; one that has
    // none makes the count 1 (§7.2.3.2 counts only definite sizes).
    let outside: Option<f32> = list.tracks.iter().map(fixed).sum();
    let one: Option<f32> = repeated
        .iter()
        .map(|size| fixed(size).map(|s| s.max(1.0)))
        .sum();
    let (Some(outside), Some(one)) = (outside, one) else {
        return 1;
    };
    let gaps_outside = room.gap * list.tracks.len() as f32;
    let per_repeat = one + room.gap * repeated.len() as f32;
    let fit = |space: f32| {
        // n repetitions and the tracks outside them, with a gap between each
        // pair of tracks: n·per_repeat + outside + gaps − one gap.
        let left = space - outside - gaps_outside + room.gap;
        (left / per_repeat).floor().max(1.0)
    };
    let count = match (room.size, room.limits) {
        (Some(size), _) => fit(size),
        (None, limits) if limits.max.is_finite() => fit(limits.max),
        (None, limits) if limits.min > 0.0 => {
            let left = limits.min - outside - gaps_outside + room.gap;
            (left / per_repeat).ceil().max(1.0)
        }
        (None, _) => 1.0,
    };
    (count as usize).clamp(1, GRID_LIMIT as usize)
}

/// The `index`th implicit track after the explicit grid, or before it when
/// negative, cycling through `grid-auto-*` (§7.6): forwards from the first
/// after it, backwards from the last before it.
pub(super) fn implicit_track(auto: &[TrackSize], index: isize) -> TrackSize {
    if auto.is_empty() {
        return TrackSize::AUTO;
    }
    let count = auto.len() as isize;
    auto[index.rem_euclid(count) as usize].clone()
}

#[cfg(test)]
mod tests {
    use otlyra_css::{AutoRepeat, Length, RepeatKind};

    use super::*;

    fn px(size: f32) -> TrackSize {
        TrackSize {
            min: TrackMin::Length(Length::Px(size)),
            max: TrackMax::Length(Length::Px(size)),
        }
    }

    fn name(text: &str) -> LineName {
        LineName(text.into())
    }

    fn room(size: f32) -> Room {
        Room {
            size: Some(size),
            limits: Limits::NONE,
            gap: 0.0,
        }
    }

    /// `100px repeat(auto-fill, 50px) 100px` in 400 goes in four times, in
    /// its place.
    #[test]
    fn an_automatic_repetition_goes_in_where_it_was_written() {
        let list = TrackList {
            tracks: vec![px(100.0), px(100.0)],
            names: vec![Vec::new(); 3],
            auto_repeat: Some(AutoRepeat {
                at: 1,
                kind: RepeatKind::Fill,
                tracks: vec![px(50.0)],
                names: vec![Vec::new(); 2],
                after: Vec::new(),
            }),
        };
        let axis = ExplicitAxis::new(&list, &[], 0, &[TrackSize::AUTO], room(400.0));
        let sizes: Vec<TrackSize> = [100.0, 50.0, 50.0, 50.0, 50.0, 100.0].map(px).to_vec();
        assert_eq!(axis.tracks, sizes);
        assert_eq!(axis.repeated, Some(1..5));
    }

    /// Every copy of a repetition names its lines, so `col 2` is the second.
    #[test]
    fn every_repetition_names_its_lines() {
        let list = TrackList {
            tracks: Vec::new(),
            names: vec![Vec::new()],
            auto_repeat: Some(AutoRepeat {
                at: 0,
                kind: RepeatKind::Fill,
                tracks: vec![px(100.0)],
                names: vec![vec![name("col")], Vec::new()],
                after: Vec::new(),
            }),
        };
        let axis = ExplicitAxis::new(&list, &[], 0, &[TrackSize::AUTO], room(350.0));
        assert_eq!(axis.tracks.len(), 3);
        assert_eq!(axis.names.lines(&name("col")), [0, 1, 2]);
    }

    /// With no size of its own a grid repeats as few times as reach its
    /// minimum, and once with neither.
    #[test]
    fn an_indefinite_grid_counts_against_its_minimum() {
        let list = TrackList {
            tracks: Vec::new(),
            names: vec![Vec::new()],
            auto_repeat: Some(AutoRepeat {
                at: 0,
                kind: RepeatKind::Fill,
                tracks: vec![px(50.0)],
                names: vec![Vec::new(); 2],
                after: Vec::new(),
            }),
        };
        let at_least = |min: f32| {
            ExplicitAxis::new(
                &list,
                &[],
                0,
                &[TrackSize::AUTO],
                Room {
                    size: None,
                    limits: Limits {
                        min,
                        max: f32::INFINITY,
                    },
                    gap: 0.0,
                },
            )
            .tracks
            .len()
        };
        assert_eq!(at_least(120.0), 3);
        assert_eq!(at_least(0.0), 1);
    }

    /// Implicit tracks cycle forwards after the grid and backwards before it.
    #[test]
    fn implicit_tracks_cycle() {
        let auto = [px(10.0), px(20.0)];
        assert_eq!(implicit_track(&auto, 0), px(10.0));
        assert_eq!(implicit_track(&auto, 3), px(20.0));
        assert_eq!(implicit_track(&auto, -1), px(20.0));
    }
}
