//! Resolving a point to whatever was drawn there.
//!
//! Hit testing reads the display list rather than a tree of its own, because a
//! second structure is a second thing to keep in step — and the failure mode when
//! it drifts is a link that is clickable somewhere other than where it is drawn.

use kurbo::Point;

use crate::display_list::{DisplayItem, DisplayList, HitTestId};

/// What was hit, and by which item.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The identifier the item carried.
    pub id: HitTestId,
    /// The item's index in the list, so a caller can tell two hits at the same id
    /// apart.
    pub index: usize,
}

/// The topmost hit-test item covering `point`, or `None`.
///
/// Walks the list backwards: the display list is in paint order, so the last item
/// covering a point is the one drawn over all the others, and that is the one the
/// user believes they clicked.
pub fn hit_test(list: &DisplayList, point: impl Into<Point>) -> Option<Hit> {
    let point = point.into();
    list.items()
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| match item {
            DisplayItem::HitTest {
                rect,
                transform,
                id,
            } => {
                // The rectangle is in the item's own space, so the point comes back
                // to meet it. A transform that collapses to nothing inverts to a
                // non-finite matrix, and a point through that lands nowhere real,
                // which the containment check then rejects.
                let local = transform.inverse() * point;
                rect.contains(local).then_some(Hit { id: *id, index })
            }
            _ => None,
        })
}

/// Every hit-test item covering `point`, topmost first.
///
/// The plural form exists because an element is usually inside another one, and
/// "what did I click" and "what is this inside" are different questions.
pub fn hit_test_all(list: &DisplayList, point: impl Into<Point>) -> Vec<Hit> {
    let point = point.into();
    list.items()
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, item)| match item {
            DisplayItem::HitTest {
                rect,
                transform,
                id,
            } => {
                let local = transform.inverse() * point;
                rect.contains(local).then_some(Hit { id: *id, index })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use kurbo::{Affine, Rect, Shape};
    use peniko::{Brush, Color, Fill};

    use super::*;
    use crate::display_list::DisplayItem;

    fn area(list: &mut DisplayList, x: f64, y: f64, width: f64, height: f64, id: u64) {
        list.push(DisplayItem::HitTest {
            rect: Rect::new(x, y, x + width, y + height),
            transform: Affine::IDENTITY,
            id: HitTestId(id),
        });
    }

    #[test]
    fn a_point_inside_an_area_hits_it() {
        let mut list = DisplayList::new();
        area(&mut list, 10.0, 10.0, 100.0, 20.0, 7);

        assert_eq!(hit_test(&list, (50.0, 20.0)).map(|hit| hit.id.0), Some(7));
        assert_eq!(hit_test(&list, (5.0, 20.0)), None);
        assert_eq!(hit_test(&list, (50.0, 40.0)), None);
    }

    /// The list is in paint order, so the last item covering a point is the one on
    /// top — and the one the user believes they clicked.
    #[test]
    fn overlapping_areas_resolve_to_the_one_painted_last() {
        let mut list = DisplayList::new();
        area(&mut list, 0.0, 0.0, 100.0, 100.0, 1);
        area(&mut list, 40.0, 40.0, 20.0, 20.0, 2);

        assert_eq!(hit_test(&list, (50.0, 50.0)).map(|hit| hit.id.0), Some(2));
        assert_eq!(hit_test(&list, (10.0, 10.0)).map(|hit| hit.id.0), Some(1));

        let all: Vec<u64> = hit_test_all(&list, (50.0, 50.0))
            .into_iter()
            .map(|hit| hit.id.0)
            .collect();
        assert_eq!(all, vec![2, 1], "topmost first, then what it sits in");
    }

    #[test]
    fn a_transform_moves_the_target_with_the_drawing() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::HitTest {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            transform: Affine::scale(2.0) * Affine::translate((5.0, 5.0)),
            id: HitTestId(3),
        });

        // The area is drawn from (10, 10) to (30, 30) once the transform is applied.
        assert_eq!(hit_test(&list, (20.0, 20.0)).map(|hit| hit.id.0), Some(3));
        assert_eq!(hit_test(&list, (5.0, 5.0)), None);
    }

    #[test]
    fn items_that_are_not_hit_test_items_are_ignored() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Solid(Color::BLACK),
            brush_transform: None,
            shape: Rect::new(0.0, 0.0, 100.0, 100.0).to_path(0.1),
        });
        assert_eq!(
            hit_test(&list, (50.0, 50.0)),
            None,
            "a fill is not a target"
        );
    }

    #[test]
    fn an_empty_list_hits_nothing() {
        assert_eq!(hit_test(&DisplayList::new(), (0.0, 0.0)), None);
    }
}
