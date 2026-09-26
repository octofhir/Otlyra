//! The grid properties, from the style engine's computed values into
//! [`crate::grid`]'s.

use std::num::{NonZeroI32, NonZeroU32};
use std::sync::Arc;

use style::properties::ComputedValues;
use style::values::CustomIdent;
use style::values::computed::{
    GridLine as StyloLine, GridTemplateComponent, ImplicitGridTracks, TrackBreadth,
    TrackList as StyloTrackList, TrackSize as StyloTrackSize,
};
use style::values::generics::grid::{GenericTrackListValue as ListValue, RepeatCount};
use style::values::specified::position::{GridAutoFlow as StyloFlow, GridTemplateAreas};

use super::length_percentage;
use crate::grid::{
    AutoRepeat, FlowAxis, GridAutoFlow, GridLine, GridPlacement, GridTemplate, LineName, NamedArea,
    RepeatKind, TrackList, TrackMax, TrackMin, TrackSize,
};

/// A grid container's whole template.
pub(super) fn grid_template(values: &ComputedValues) -> GridTemplate {
    let position = values.get_position();
    let (areas, area_rows, area_columns) = areas(&position.grid_template_areas);
    GridTemplate {
        columns: template(&position.grid_template_columns),
        rows: template(&position.grid_template_rows),
        areas,
        area_rows,
        area_columns,
        auto_columns: implicit(&position.grid_auto_columns),
        auto_rows: implicit(&position.grid_auto_rows),
        flow: auto_flow(position.grid_auto_flow),
    }
}

/// An item's placement along one axis, from its two longhands.
pub(super) fn placement(start: &StyloLine, end: &StyloLine) -> GridPlacement {
    GridPlacement {
        start: line(start),
        end: line(end),
    }
}

/// A `grid-template-columns` or `-rows`.
///
/// `subgrid` never parses in this engine's servo mode (Stylo's
/// `allow_grid_template_subgrids` is false there), and `masonry` is Gecko's
/// alone; either would be `none` here, which is what a browser without them
/// makes of the declaration.
fn template(value: &GridTemplateComponent) -> TrackList {
    match value {
        GridTemplateComponent::None
        | GridTemplateComponent::Subgrid(_)
        | GridTemplateComponent::Masonry => TrackList::default(),
        GridTemplateComponent::TrackList(list) => track_list(list),
    }
}

/// The tracks and line names of a list, with each `repeat()` of a count
/// written out and names that fall on one line merged there (§7.2.2,
/// §7.2.3.1).
fn track_list(list: &StyloTrackList) -> TrackList {
    let mut tracks = Vec::new();
    let mut names: Vec<Vec<LineName>> = vec![Vec::new()];
    let mut auto_repeat: Option<AutoRepeat> = None;
    // Whether the next names belong on the line after an automatic repetition,
    // which is not a line of the list as written.
    let mut after_repeat = false;

    for (index, value) in list.values.iter().enumerate() {
        let before = idents(list.line_names.get(index));
        match (after_repeat, auto_repeat.as_mut()) {
            (true, Some(repeat)) => repeat.after.extend(before),
            _ => last_line(&mut names).extend(before),
        }
        after_repeat = false;

        match value {
            ListValue::TrackSize(size) => {
                tracks.push(track_size(size));
                names.push(Vec::new());
            }
            ListValue::TrackRepeat(repeat) => match repeat.count {
                RepeatCount::Number(count) => {
                    for _ in 0..count.max(0) {
                        for (at, size) in repeat.track_sizes.iter().enumerate() {
                            last_line(&mut names).extend(idents(repeat.line_names.get(at)));
                            tracks.push(track_size(size));
                            names.push(Vec::new());
                        }
                        last_line(&mut names)
                            .extend(idents(repeat.line_names.get(repeat.track_sizes.len())));
                    }
                }
                RepeatCount::AutoFill | RepeatCount::AutoFit => {
                    auto_repeat = Some(AutoRepeat {
                        at: tracks.len(),
                        kind: if matches!(repeat.count, RepeatCount::AutoFit) {
                            RepeatKind::Fit
                        } else {
                            RepeatKind::Fill
                        },
                        tracks: repeat.track_sizes.iter().map(track_size).collect(),
                        names: repeat
                            .line_names
                            .iter()
                            .map(|names| names.iter().map(name).collect())
                            .collect(),
                        after: Vec::new(),
                    });
                    after_repeat = true;
                }
            },
        }
    }

    let last = idents(list.line_names.get(list.values.len()));
    match (after_repeat, auto_repeat.as_mut()) {
        (true, Some(repeat)) => repeat.after.extend(last),
        _ => last_line(&mut names).extend(last),
    }

    TrackList {
        tracks,
        names,
        auto_repeat,
    }
}

/// The line the next names go on: the last one so far.
fn last_line(names: &mut Vec<Vec<LineName>>) -> &mut Vec<LineName> {
    if names.is_empty() {
        names.push(Vec::new());
    }
    let last = names.len() - 1;
    &mut names[last]
}

fn idents(names: Option<&style::OwnedSlice<CustomIdent>>) -> Vec<LineName> {
    names.map_or_else(Vec::new, |names| names.iter().map(name).collect())
}

fn name(ident: &CustomIdent) -> LineName {
    LineName(Arc::from(&*ident.0))
}

/// One `<track-size>` as its two sizing functions (§7.2.1).
fn track_size(size: &StyloTrackSize) -> TrackSize {
    match size {
        StyloTrackSize::Breadth(breadth) => TrackSize {
            min: minimum(breadth),
            max: maximum(breadth),
        },
        StyloTrackSize::Minmax(min, max) => TrackSize {
            min: minimum(min),
            max: maximum(max),
        },
        StyloTrackSize::FitContent(limit) => TrackSize {
            min: TrackMin::Auto,
            max: match limit {
                TrackBreadth::Breadth(length) => TrackMax::FitContent(length_percentage(length)),
                // `fit-content()` takes a `<length-percentage>` and nothing
                // else parses; were one of these here it would be itself.
                TrackBreadth::Flex(_)
                | TrackBreadth::Auto
                | TrackBreadth::MinContent
                | TrackBreadth::MaxContent => maximum(limit),
            },
        },
    }
}

/// A breadth in the minimum position. A flexible one is `auto` there: `1fr`
/// alone is `minmax(auto, 1fr)`, and `minmax(1fr, …)` does not parse
/// (§7.2.4).
fn minimum(breadth: &TrackBreadth) -> TrackMin {
    match breadth {
        TrackBreadth::Breadth(length) => TrackMin::Length(length_percentage(length)),
        TrackBreadth::Flex(_) | TrackBreadth::Auto => TrackMin::Auto,
        TrackBreadth::MinContent => TrackMin::MinContent,
        TrackBreadth::MaxContent => TrackMin::MaxContent,
    }
}

fn maximum(breadth: &TrackBreadth) -> TrackMax {
    match breadth {
        TrackBreadth::Breadth(length) => TrackMax::Length(length_percentage(length)),
        TrackBreadth::Flex(flex) => TrackMax::Fr(flex.0),
        TrackBreadth::Auto => TrackMax::Auto,
        TrackBreadth::MinContent => TrackMax::MinContent,
        TrackBreadth::MaxContent => TrackMax::MaxContent,
    }
}

/// `grid-auto-columns` or `-rows`, which is never empty: the initial `auto`
/// is one `auto` track.
fn implicit(tracks: &ImplicitGridTracks) -> Vec<TrackSize> {
    if tracks.0.is_empty() {
        vec![TrackSize::AUTO]
    } else {
        tracks.0.iter().map(track_size).collect()
    }
}

/// The named areas, as track ranges counting from zero — the engine's count
/// lines from one — and how many rows and columns they take.
fn areas(value: &GridTemplateAreas) -> (Vec<NamedArea>, u32, u32) {
    match value {
        GridTemplateAreas::None => (Vec::new(), 0, 0),
        GridTemplateAreas::Areas(areas) => {
            let areas = &areas.0;
            let named = areas
                .areas
                .iter()
                .map(|area| NamedArea {
                    name: LineName(Arc::from(&*area.name)),
                    rows: area.rows.start.saturating_sub(1)..area.rows.end.saturating_sub(1),
                    columns: area.columns.start.saturating_sub(1)
                        ..area.columns.end.saturating_sub(1),
                })
                .collect();
            (
                named,
                u32::try_from(areas.strings.len()).unwrap_or(u32::MAX),
                areas.width,
            )
        }
    }
}

fn auto_flow(flow: StyloFlow) -> GridAutoFlow {
    GridAutoFlow {
        axis: if flow.contains(StyloFlow::COLUMN) {
            FlowAxis::Column
        } else {
            FlowAxis::Row
        },
        dense: flow.contains(StyloFlow::DENSE),
    }
}

/// One edge: a span, a numbered line with or without a name, a bare name, or
/// `auto` (§8.3). A span of a name alone is a span of one such line.
fn line(value: &StyloLine) -> GridLine {
    let named = (!value.ident.0.is_empty()).then(|| name(&value.ident));
    if value.is_span {
        let count = u32::try_from(value.line_num)
            .ok()
            .and_then(NonZeroU32::new)
            .unwrap_or(NonZeroU32::MIN);
        return GridLine::Span { count, name: named };
    }
    match (NonZeroI32::new(value.line_num), named) {
        (Some(nth), name) => GridLine::Line { nth, name },
        (None, Some(name)) => GridLine::Area(name),
        (None, None) => GridLine::Auto,
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroI32, NonZeroU32};

    use crate::computed::tests::layout_style;
    use crate::grid::{
        FlowAxis, GridLine, GridTemplate, LineName, NamedArea, RepeatKind, TrackMax, TrackMin,
        TrackSize,
    };
    use crate::style::{AlignItems, JustifyContent, Length};

    fn grid(css: &str) -> GridTemplate {
        let style = layout_style(
            &format!("<style>div {{ display: grid; {css} }}</style><div>x</div>"),
            "div",
        );
        (*style.grid.expect("a grid container")).clone()
    }

    fn name(text: &str) -> LineName {
        LineName(text.into())
    }

    fn names(texts: &[&str]) -> Vec<LineName> {
        texts.iter().map(|text| name(text)).collect()
    }

    fn px(size: f32) -> TrackSize {
        TrackSize {
            min: TrackMin::Length(Length::Px(size)),
            max: TrackMax::Length(Length::Px(size)),
        }
    }

    #[test]
    fn a_track_list_keeps_its_names_and_its_automatic_repetition() {
        let template = grid(
            // Beside an automatic repetition every track has to be fixed.
            "grid-template-columns: [a] 10px [b] minmax(100px, 200px) \
             repeat(auto-fit, [c] 50px [d]) [e] 100px [f]",
        );
        let columns = template.columns;
        assert_eq!(columns.tracks.len(), 3);
        assert_eq!(
            columns.names,
            [names(&["a"]), names(&["b"]), Vec::new(), names(&["f"])]
        );
        let repeat = columns.auto_repeat.expect("an automatic repetition");
        assert_eq!((repeat.at, repeat.kind), (2, RepeatKind::Fit));
        assert_eq!(repeat.tracks, [px(50.0)]);
        assert_eq!(repeat.names, [names(&["c"]), names(&["d"])]);
        assert_eq!(repeat.after, names(&["e"]));
    }

    #[test]
    fn names_on_one_line_merge_across_repetitions() {
        let columns = grid("grid-template-columns: repeat(2, [x] 10px [y])").columns;
        assert_eq!(
            columns.names,
            [names(&["x"]), names(&["y", "x"]), names(&["y"])]
        );
    }

    #[test]
    fn every_track_size_is_a_minimum_and_a_maximum() {
        let columns =
            grid("grid-template-columns: fit-content(100px) minmax(25px, min-content) 1fr auto")
                .columns;
        assert_eq!(
            columns.tracks,
            [
                TrackSize {
                    min: TrackMin::Auto,
                    max: TrackMax::FitContent(Length::Px(100.0)),
                },
                TrackSize {
                    min: TrackMin::Length(Length::Px(25.0)),
                    max: TrackMax::MinContent,
                },
                TrackSize {
                    min: TrackMin::Auto,
                    max: TrackMax::Fr(1.0),
                },
                TrackSize::AUTO,
            ]
        );
    }

    #[test]
    fn placements_keep_lines_names_and_spans() {
        let item = |css: &str| layout_style(&format!("<style>p {{ {css} }}</style><p>x"), "p");
        let whole = item("grid-column: 1 / -1").grid_column;
        let line = |nth: i32| GridLine::Line {
            nth: NonZeroI32::new(nth).expect("a line"),
            name: None,
        };
        assert_eq!((whole.start, whole.end), (line(1), line(-1)));

        let main = item("grid-area: main");
        let area = GridLine::Area(name("main"));
        assert_eq!(main.grid_row.start, area);
        assert_eq!(main.grid_column.end, area);

        let span = |count: u32| GridLine::Span {
            count: NonZeroU32::new(count).expect("a span"),
            name: Some(name("foo")),
        };
        assert_eq!(item("grid-column: span 2 foo").grid_column.start, span(2));
        assert_eq!(item("grid-column: span foo").grid_column.start, span(1));
    }

    #[test]
    fn areas_are_track_ranges_counted_from_zero() {
        let template = grid("grid-template-areas: 'h h' 's m'");
        assert_eq!((template.area_rows, template.area_columns), (2, 2));
        let area = |text: &str| {
            template
                .areas
                .iter()
                .find(|area| area.name == name(text))
                .cloned()
                .expect("the area")
        };
        assert_eq!(
            area("h"),
            NamedArea {
                name: name("h"),
                rows: 0..1,
                columns: 0..2,
            }
        );
        assert_eq!(area("m").columns, 1..2);
        assert_eq!(area("s").rows, 1..2);
    }

    #[test]
    fn the_flow_and_the_implicit_tracks() {
        let template = grid("grid-auto-flow: column dense; grid-auto-rows: 10px auto");
        assert_eq!(template.flow.axis, FlowAxis::Column);
        assert!(template.flow.dense);
        assert_eq!(template.auto_rows, [px(10.0), TrackSize::AUTO]);
        assert_eq!(template.auto_columns, [TrackSize::AUTO]);
    }

    #[test]
    fn alignment_reads_normal_legacy_and_left_right() {
        let style = |css: &str| layout_style(&format!("<style>p {{ {css} }}</style><p>x"), "p");
        let centred = style("place-items: center");
        assert_eq!(
            (centred.justify_items, centred.align_items),
            (AlignItems::Center, AlignItems::Center)
        );
        assert_eq!(
            style("justify-items: legacy").justify_items,
            AlignItems::Normal
        );
        assert_eq!(style("justify-self: auto").justify_self, None);
        assert_eq!(
            style("justify-self: right").justify_self,
            Some(AlignItems::End)
        );
        let initial = style("");
        assert_eq!(initial.align_items, AlignItems::Normal);
        assert_eq!(initial.justify_content, JustifyContent::Stretch);
    }

    #[test]
    fn a_track_list_is_written_back_as_css() {
        let columns =
            grid("grid-template-columns: [a] 100px repeat(auto-fill, minmax(50px, 1fr)) [b]")
                .columns;
        assert_eq!(
            columns.to_string(),
            "[a] 100px repeat(auto-fill, minmax(50px, 1fr)) [b]"
        );
    }
}
