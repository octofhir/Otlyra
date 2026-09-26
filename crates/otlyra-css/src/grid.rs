//! A grid container's template and a grid item's placement, as computed values
//! (CSS Grid 2 §7, §8).
//!
//! Everything a grid needs that the cascade can settle: the explicit tracks
//! with their line names, a `repeat(auto-fill | auto-fit)` kept whole for the
//! container to count, the named areas, the implicit tracks and the flow. How
//! many times an automatic repetition goes in, and which line a name finally
//! lands on, depend on the container's size and contents, and are layout's.

use std::fmt;
use std::num::{NonZeroI32, NonZeroU32};
use std::ops::Range;
use std::sync::Arc;

use crate::style::Length;

/// An author's `<custom-ident>` naming a grid line or a grid area.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LineName(pub Arc<str>);

impl LineName {
    /// The name with `-start` or `-end` on the end: the lines a named area
    /// implies (§7.3.2), and the first thing a bare name is looked up as (§8.3).
    pub fn suffixed(&self, suffix: &str) -> Self {
        Self(Arc::from(format!("{}{suffix}", self.0)))
    }
}

impl fmt::Display for LineName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A track's minimum sizing function (§7.2.1). A flexible minimum is invalid
/// (§7.2.4), so there is none.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackMin {
    /// A length or a percentage of the grid's size along the axis.
    Length(Length),
    /// The largest minimum size of what is in it.
    Auto,
    /// The largest min-content contribution of what is in it.
    MinContent,
    /// The largest max-content contribution of what is in it.
    MaxContent,
}

/// A track's maximum sizing function (§7.2.1).
#[derive(Clone, Debug, PartialEq)]
pub enum TrackMax {
    /// A length or a percentage of the grid's size along the axis.
    Length(Length),
    /// A share of the space left, in `fr`.
    Fr(f32),
    /// The largest max-content contribution, and stretched with what is left.
    Auto,
    /// The largest min-content contribution of what is in it.
    MinContent,
    /// The largest max-content contribution of what is in it.
    MaxContent,
    /// `fit-content(<length-percentage>)`: max-content, held at the argument.
    FitContent(Length),
}

/// One track's sizing functions, a minimum and a maximum.
///
/// Every `<track-size>` comes to these two (§7.2.1): `auto` is `minmax(auto,
/// auto)`, a length is itself at both ends, `1fr` is `minmax(auto, 1fr)`,
/// `fit-content(L)` is `minmax(auto, fit-content(L))`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackSize {
    /// The smallest the track may be.
    pub min: TrackMin,
    /// The largest it grows to, or how it shares out what is left.
    pub max: TrackMax,
}

impl TrackSize {
    /// `auto`, the initial value of `grid-auto-columns` and `grid-auto-rows`.
    pub const AUTO: Self = Self {
        min: TrackMin::Auto,
        max: TrackMax::Auto,
    };
}

impl fmt::Display for TrackSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let min = |f: &mut fmt::Formatter<'_>| match &self.min {
            TrackMin::Length(length) => write!(f, "{length}"),
            TrackMin::Auto => f.write_str("auto"),
            TrackMin::MinContent => f.write_str("min-content"),
            TrackMin::MaxContent => f.write_str("max-content"),
        };
        match (&self.min, &self.max) {
            (TrackMin::Auto, TrackMax::Auto) => f.write_str("auto"),
            (TrackMin::MinContent, TrackMax::MinContent) => f.write_str("min-content"),
            (TrackMin::MaxContent, TrackMax::MaxContent) => f.write_str("max-content"),
            (TrackMin::Length(min), TrackMax::Length(max)) if min == max => write!(f, "{min}"),
            (TrackMin::Auto, TrackMax::Fr(fr)) => write!(f, "{fr}fr"),
            (TrackMin::Auto, TrackMax::FitContent(limit)) => write!(f, "fit-content({limit})"),
            (_, max) => {
                f.write_str("minmax(")?;
                min(f)?;
                f.write_str(", ")?;
                match max {
                    TrackMax::Length(length) => write!(f, "{length}")?,
                    TrackMax::Fr(fr) => write!(f, "{fr}fr")?,
                    TrackMax::Auto => f.write_str("auto")?,
                    TrackMax::MinContent => f.write_str("min-content")?,
                    TrackMax::MaxContent => f.write_str("max-content")?,
                    TrackMax::FitContent(limit) => write!(f, "fit-content({limit})")?,
                }
                f.write_str(")")
            }
        }
    }
}

/// Whether an automatic repetition keeps the tracks nothing is placed in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RepeatKind {
    /// `auto-fill`: every repetition that fits stays.
    Fill,
    /// `auto-fit`: the empty ones collapse (§7.2.3.2).
    Fit,
}

/// A `repeat(auto-fill | auto-fit, …)`, kept whole: how many times it goes in
/// depends on the container's size (§7.2.3.2).
#[derive(Clone, Debug, PartialEq)]
pub struct AutoRepeat {
    /// Where in [`TrackList::tracks`] the repetitions go.
    pub at: usize,
    /// Whether empty repetitions stay or collapse.
    pub kind: RepeatKind,
    /// The tracks of one repetition.
    pub tracks: Vec<TrackSize>,
    /// The names on the lines of one repetition, one more than its tracks.
    pub names: Vec<Vec<LineName>>,
    /// The names on the line after the last repetition. The line before the
    /// first is the list's own `names[at]`.
    pub after: Vec<LineName>,
}

/// A `grid-template-columns` or `grid-template-rows`: the explicit tracks
/// along one axis and the names of the lines between them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackList {
    /// The tracks, with every `repeat()` of a count written out.
    pub tracks: Vec<TrackSize>,
    /// The names on each line, one more entry than there are tracks — or none
    /// at all for `none`. Names that land on one line are merged there
    /// (§7.2.2): the last line of one repetition is the first of the next.
    pub names: Vec<Vec<LineName>>,
    /// The automatic repetition, if the list has one.
    pub auto_repeat: Option<AutoRepeat>,
}

impl TrackList {
    /// The names on line `index` of the list as written, without an
    /// automatic repetition.
    pub fn names_on(&self, index: usize) -> &[LineName] {
        self.names.get(index).map_or(&[], Vec::as_slice)
    }
}

impl fmt::Display for TrackList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.tracks.is_empty() && self.auto_repeat.is_none() {
            return f.write_str("none");
        }
        let mut tokens = Vec::new();
        for index in 0..=self.tracks.len() {
            push_names(&mut tokens, self.names_on(index));
            if let Some(repeat) = self
                .auto_repeat
                .as_ref()
                .filter(|repeat| repeat.at == index)
            {
                tokens.push(repeat.to_string());
                push_names(&mut tokens, &repeat.after);
            }
            if let Some(track) = self.tracks.get(index) {
                tokens.push(track.to_string());
            }
        }
        f.write_str(&tokens.join(" "))
    }
}

impl fmt::Display for AutoRepeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            RepeatKind::Fill => "auto-fill",
            RepeatKind::Fit => "auto-fit",
        };
        let mut tokens = Vec::new();
        for index in 0..=self.tracks.len() {
            push_names(
                &mut tokens,
                self.names.get(index).map_or(&[], Vec::as_slice),
            );
            if let Some(track) = self.tracks.get(index) {
                tokens.push(track.to_string());
            }
        }
        write!(f, "repeat({kind}, {})", tokens.join(" "))
    }
}

/// A line's names as CSS writes them, `[a b]`, when it has any.
fn push_names(tokens: &mut Vec<String>, names: &[LineName]) {
    if !names.is_empty() {
        let names: Vec<String> = names.iter().map(LineName::to_string).collect();
        tokens.push(format!("[{}]", names.join(" ")));
    }
}

/// One rectangle of `grid-template-areas`, as the tracks it covers, counting
/// from zero.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedArea {
    /// The area's name, and the root of the line names it implies.
    pub name: LineName,
    /// The rows it covers.
    pub rows: Range<u32>,
    /// The columns it covers.
    pub columns: Range<u32>,
}

/// Which axis the auto-placement cursor moves along.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlowAxis {
    /// Across each row, then down to the next.
    Row,
    /// Down each column, then across to the next.
    Column,
}

/// `grid-auto-flow` (§7.7).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GridAutoFlow {
    /// The axis the cursor moves along first.
    pub axis: FlowAxis,
    /// Whether a later item may fill a hole an earlier one left.
    pub dense: bool,
}

/// Everything a grid container says about its grid.
#[derive(Clone, Debug, PartialEq)]
pub struct GridTemplate {
    /// `grid-template-columns`.
    pub columns: TrackList,
    /// `grid-template-rows`.
    pub rows: TrackList,
    /// The named areas of `grid-template-areas`.
    pub areas: Vec<NamedArea>,
    /// How many rows and columns the areas take, which the explicit grid is at
    /// least (§7.1).
    pub area_rows: u32,
    /// The same across.
    pub area_columns: u32,
    /// `grid-auto-columns`: the implicit columns' sizes, in the order they
    /// cycle through (§7.6). Never empty.
    pub auto_columns: Vec<TrackSize>,
    /// `grid-auto-rows`, the same.
    pub auto_rows: Vec<TrackSize>,
    /// `grid-auto-flow`.
    pub flow: GridAutoFlow,
}

impl Default for GridTemplate {
    /// The initial values: no explicit tracks, `auto` implicit ones, placed
    /// row by row.
    fn default() -> Self {
        Self {
            columns: TrackList::default(),
            rows: TrackList::default(),
            areas: Vec::new(),
            area_rows: 0,
            area_columns: 0,
            auto_columns: vec![TrackSize::AUTO],
            auto_rows: vec![TrackSize::AUTO],
            flow: GridAutoFlow {
                axis: FlowAxis::Row,
                dense: false,
            },
        }
    }
}

/// One edge of a grid item's placement (§8.3).
#[derive(Clone, Debug, PartialEq)]
pub enum GridLine {
    /// Placed by the auto-placement algorithm.
    Auto,
    /// A bare name: the area's `<name>-start` or `<name>-end` line if there is
    /// one, the first line called `<name>` otherwise.
    Area(LineName),
    /// The `nth` line, counting from the start or, when negative, from the end
    /// — only lines with that name, when it has one.
    Line {
        /// Which line, from the start when positive and the end when negative.
        nth: NonZeroI32,
        /// The name the lines counted carry, if only some count.
        name: Option<LineName>,
    },
    /// So many tracks from the other edge, or as far as the `count`th line with
    /// that name.
    Span {
        /// How many tracks, or how many lines with the name.
        count: NonZeroU32,
        /// The name the lines counted carry, if only some count.
        name: Option<LineName>,
    },
}

impl fmt::Display for GridLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Area(name) => write!(f, "{name}"),
            Self::Line { nth, name: None } => write!(f, "{nth}"),
            Self::Line {
                nth,
                name: Some(name),
            } => write!(f, "{nth} {name}"),
            Self::Span { count, name: None } => write!(f, "span {count}"),
            Self::Span {
                count,
                name: Some(name),
            } => write!(f, "span {count} {name}"),
        }
    }
}

/// A grid item's two edges along one axis.
#[derive(Clone, Debug, PartialEq)]
pub struct GridPlacement {
    /// `grid-*-start`.
    pub start: GridLine,
    /// `grid-*-end`.
    pub end: GridLine,
}

impl GridPlacement {
    /// Wherever auto-placement puts it, one track wide.
    pub const AUTO: Self = Self {
        start: GridLine::Auto,
        end: GridLine::Auto,
    };
}
