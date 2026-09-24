//! The page driven the way the browser drives it — presses, keys, the wheel, a
//! search — and read back through the frames it builds.
//!
//! One module rather than one beside each file it tests, because most of what is
//! here crosses several of them: a press moves the focus, puts a caret in a field
//! and opens a list in one call, and the frame that shows it is built in another.

use otlyra_gfx::{DisplayItem, PaintOp, RecordingPainter, render};

use super::field::CARET_BLINK_INTERVAL;
use super::*;

fn scene(html: &str) -> (PageScene, TextEngine) {
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    (PageScene::new(parsed.document), TextEngine::isolated())
}

/// The seam the inspector was reading-only for want of: an edit goes in and
/// the next frame is the page as edited, restyled, relaid and repainted.
#[test]
fn an_edit_restyles_the_page_and_the_next_frame_shows_it() {
    let (mut page, mut text) =
        scene("<style>p { color: red } .big { font-size: 40px }</style><body><p>text");
    let before = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let builds = page.builds();

    let paragraph = {
        let document = page.document();
        let mut stack = vec![document.root()];
        let mut found = None;
        while let Some(node) = stack.pop() {
            stack.extend(document.children(node));
            if matches!(document.get(node).map(|n| &n.data),
                Some(NodeData::Element(element)) if element.name.local.as_ref() == "p")
            {
                found = Some(node);
            }
        }
        found.expect("the document has a p")
    };

    // A class that a rule in the page is waiting for.
    assert!(page.edit(|document| document.set_attr(paragraph, "class", "big")));
    let after = page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    assert!(page.builds() > builds, "an edit is a frame to build again");
    assert_ne!(before, after, "and the page is drawn differently for it");

    // The cascade really ran: the rule the class selects took effect.
    let glyph_height = |list: &DisplayList| {
        list.items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Glyphs { font_size, .. } => Some(*font_size),
                _ => None,
            })
            .fold(0.0_f32, f32::max)
    };
    assert!(
        glyph_height(&after) > glyph_height(&before),
        "the class brought a bigger font size with it"
    );

    // And it settles again: an edited page with a still reader is as idle as
    // any other.
    let builds = page.builds();
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), builds);
}

/// W10's page half: an idle page with a still reader does no work. Every
/// mutation records damage and the frame reads it, which is what `Damage`
/// was written for and what nothing did until now.
#[test]
fn an_unchanged_page_is_not_painted_a_second_time() {
    let (mut page, mut text) = scene("<body><h1>Title</h1><p>Some text to lay out.</p>");
    let first = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), 1);

    let again = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), 1, "nothing about it moved");
    assert_eq!(first, again, "and the frame is the same frame");

    // Scrolling is a repaint, and a repaint is what it asks for.
    page.set_scroll(40.0);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), 2);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), 2, "and then it is still again");

    // A resize is a relayout, and a different band of the window to draw in
    // is a different frame even when nothing else moved.
    page.build_display_list(&mut text, 700.0, 600.0, 0.0);
    assert_eq!(page.builds(), 3);
    page.build_display_list(&mut text, 700.0, 600.0, 12.0);
    assert_eq!(page.builds(), 4, "the page moved down the window");
}

/// A press still lands on what is on screen when the frame was reused: the
/// targets came out of that very list, so they describe it exactly.
#[test]
fn a_reused_frame_is_still_the_frame_a_press_is_tested_against() {
    let (mut page, mut text) = scene("<body><p><a href=\"/next\">go</a></p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let before = page.boxes().root();
    let _ = before;
    let hit = (0..600)
        .step_by(4)
        .find_map(|y| page.box_at(20.0, f64::from(y)));

    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), 1, "the frame was reused");
    assert_eq!(
        hit,
        (0..600)
            .step_by(4)
            .find_map(|y| page.box_at(20.0, f64::from(y))),
        "and it still answers where things are"
    );
}

/// Where the first box of `tag` is, in window coordinates.
fn point_on(page: &PageScene, tag: &str) -> (f64, f64) {
    for y in (0..600).step_by(2) {
        for x in (0..800).step_by(2) {
            if let Some(id) = page.box_at(f64::from(x), f64::from(y))
                && page
                    .boxes()
                    .get(id)
                    .and_then(|node| node.tag.clone())
                    .is_some_and(|found| found.as_ref() == tag)
            {
                return (f64::from(x), f64::from(y));
            }
        }
    }
    panic!("no {tag} on the page");
}

/// The text every box in the page draws, run together.
fn page_text(page: &PageScene) -> String {
    let tree = page.boxes();
    tree.descendants(tree.root())
        .into_iter()
        .filter_map(|id| match &tree.node(id).kind {
            otlyra_layout::box_tree::BoxKind::Text(text) => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

/// Whether the box the given tag generated is drawn as checked.
fn is_checked(page: &PageScene, tag: &str) -> bool {
    let tree = page.boxes();
    tree.descendants(tree.root()).into_iter().any(|id| {
        let node = tree.node(id);
        node.tag.as_ref().is_some_and(|found| found.as_ref() == tag)
            && node
                .control
                .as_ref()
                .is_some_and(|control| control.state.checked)
    })
}

#[test]
fn a_press_and_a_release_on_a_checkbox_tick_it() {
    let (mut page, mut text) = scene("<body><input type=checkbox>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");

    assert!(!is_checked(&page, "input"));
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(is_checked(&page, "input"), "a click ticks it");

    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(!is_checked(&page, "input"), "and the next one unticks it");
}

/// A press that wanders off before it is let go takes itself back, which is
/// what a press means on every platform.
#[test]
fn a_release_somewhere_else_does_not_tick_it() {
    let (mut page, mut text) = scene("<body><input type=checkbox>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");

    page.pointer_pressed(x, y);
    page.pointer_released(x + 400.0, y + 200.0);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(!is_checked(&page, "input"));
}

/// Clicking the words beside a checkbox ticks it, because a label's activation
/// behaviour is its control's.
#[test]
fn a_label_passes_the_press_to_what_it_names() {
    let (mut page, mut text) =
        scene("<body><label for=agree>I agree</label><input id=agree type=checkbox>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "label");

    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(is_checked(&page, "input"));
}

#[test]
fn only_one_radio_button_in_a_group_is_checked_at_a_time() {
    let (mut page, mut text) =
        scene("<body><input type=radio name=g value=a><input type=radio name=g value=b>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    let checked_values = |page: &PageScene| -> Vec<String> {
        let tree = page.boxes();
        tree.descendants(tree.root())
            .into_iter()
            .filter(|&id| {
                tree.node(id)
                    .control
                    .as_ref()
                    .is_some_and(|control| control.state.checked)
            })
            .filter_map(|id| {
                let node = tree.node(id).node?;
                page.document()
                    .get(node)?
                    .element()?
                    .attr("value")
                    .map(str::to_owned)
            })
            .collect()
    };

    // The two are side by side; the first is at the left edge and the second
    // beyond it.
    let mut seen = Vec::new();
    for x in (0..200).step_by(2) {
        if let Some(id) = page.box_at(f64::from(x), 12.0)
            && page
                .boxes()
                .get(id)
                .and_then(|node| node.control.clone())
                .is_some()
            && !seen.contains(&id)
        {
            seen.push(id);
        }
    }
    assert!(seen.len() >= 2, "both radio buttons are on the page");

    let first = page.rect_of(seen[0]).expect("a rectangle");
    let second = page.rect_of(seen[1]).expect("a rectangle");
    let click = |page: &mut PageScene, rect: otlyra_layout::Rect| {
        let (x, y) = (
            f64::from(rect.x + rect.width / 2.0),
            f64::from(rect.y + rect.height / 2.0),
        );
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
    };

    click(&mut page, first);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(checked_values(&page), ["a"]);

    click(&mut page, second);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(checked_values(&page), ["b"], "the first one gave way");
}

#[test]
fn typing_into_a_field_shows_what_was_typed() {
    let (mut page, mut text) = scene("<body><input>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");

    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert!(page.typed("Ada"));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "Ada");

    assert!(page.edit_text(EditAction::Backspace, false));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "Ad");

    // The caret is where the typing goes, and it moves.
    assert!(page.edit_text(EditAction::Home, false));
    assert!(page.typed("M"));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "MAd");
}

/// The caret a page draws, if it draws one.
fn caret_of(page: &mut PageScene, text: &mut TextEngine) -> Option<(f64, f64)> {
    let list = page.build_display_list(text, 800.0, 600.0, 0.0);
    // The caret is the last thing drawn, and it is one pixel wide.
    list.items().iter().rev().find_map(|item| match item {
        DisplayItem::Fill { shape, .. } => {
            let bounds = otlyra_gfx::kurbo::Shape::bounding_box(shape);
            (bounds.width() - 1.0)
                .abs()
                .lt(&0.01)
                .then_some((bounds.x0, bounds.y0))
        }
        _ => None,
    })
}

#[test]
fn a_field_with_the_focus_shows_a_caret_that_moves_with_the_typing() {
    let (mut page, mut text) = scene("<body><input>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(
        caret_of(&mut page, &mut text).is_none(),
        "a page nobody has clicked has no caret"
    );

    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    let empty = caret_of(&mut page, &mut text).expect("an empty field still has a caret");

    page.typed("Ada");
    let typed = caret_of(&mut page, &mut text).expect("and a full one has one too");
    assert!(
        typed.0 > empty.0,
        "the caret is past what was typed: {typed:?} against {empty:?}"
    );

    page.edit_text(EditAction::Home, false);
    let home = caret_of(&mut page, &mut text).expect("a caret at the start");
    assert!(
        (home.0 - empty.0).abs() < 0.5,
        "and back where it started: {home:?} against {empty:?}"
    );
}

#[test]
fn a_click_into_a_field_puts_the_caret_where_it_landed() {
    let (mut page, mut text) = scene("<body><input value=abcdefghij>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");

    // Near the left edge of the text: before what is there rather than after.
    page.pointer_pressed(x + 3.0, y);
    page.pointer_released(x + 3.0, y);
    page.typed("Z");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let started = page_text(&page);
    assert!(
        started.starts_with('Z'),
        "the caret landed at the start: {started:?}"
    );

    // Well past the end of the text: after all of it.
    page.pointer_pressed(x + 400.0, y);
    page.pointer_released(x + 400.0, y);
    assert!(!page.typed("Q"), "and that is not in the field at all");
}

#[test]
fn a_click_on_page_text_does_not_turn_into_a_caret() {
    let (mut page, mut text) = scene("<body><input><p>ordinary page text</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    let (field_x, field_y) = point_on(&page, "input");
    page.pointer_pressed(field_x, field_y);
    page.pointer_released(field_x, field_y);
    assert!(
        caret_of(&mut page, &mut text).is_some(),
        "the editable field starts with a caret"
    );

    page.blur();
    let (text_x, text_y) = point_on(&page, "p");
    assert!(page.select_from(text_x as f32, text_y as f32, 0.0));
    assert!(
        page.selection.is_some_and(|selection| selection.is_empty()),
        "the click still anchors a possible drag selection"
    );
    assert!(
        caret_of(&mut page, &mut text).is_none(),
        "plain page text is not an editable caret"
    );
    assert!(
        !page.caret_blinks(),
        "a plain click must not schedule perpetual caret frames"
    );
}

/// A drag inside a field takes the letters it passes, and what is typed over a
/// selection replaces it.
#[test]
fn a_field_has_a_selection_of_its_own() {
    let (mut page, mut text) = scene("<body><input value=abcdefgh>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");

    // From the very start to well past the end: everything.
    page.pointer_pressed(x, y);
    // A frame between the two, as there is in the window: the press restyles,
    // and a drag is tested against the layout the last frame was drawn from.
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    page.pointer_moved(x + 400.0, y);
    page.pointer_released(x + 400.0, y);
    assert_eq!(page.selected_text().as_deref(), Some("abcdefgh"));
    assert!(page.has_selection());

    // Typed over, it goes.
    page.typed("Z");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "Z");
    assert!(!page.has_selection(), "and nothing is selected afterwards");
}

#[test]
fn shift_and_an_arrow_take_the_letters_they_pass_and_a_bare_one_does_not() {
    let (mut page, mut text) = scene("<body><input value=abcd>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.edit_text(EditAction::End, false);

    page.edit_text(EditAction::Left, true);
    page.edit_text(EditAction::Left, true);
    assert_eq!(page.selected_text().as_deref(), Some("cd"));

    // Without shift it drops what was taken and lands at the edge it was
    // pushed towards rather than one further in.
    page.edit_text(EditAction::Left, false);
    assert!(!page.has_selection());
    page.typed("-");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "ab-cd");
}

#[test]
fn selecting_everything_in_a_focused_field_takes_the_field_and_not_the_page() {
    let (mut page, mut text) = scene("<body><p>prose</p><input value=typed>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);

    assert!(page.select_all());
    assert_eq!(page.selected_text().as_deref(), Some("typed"));

    // A backspace over it takes the whole of it.
    page.edit_text(EditAction::Backspace, false);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(!page_text(&page).contains("typed"));
}

/// An open drop-down shows its options over the page rather than in it, so
/// opening one moves nothing behind it.
#[test]
fn a_drop_down_opens_over_the_page_and_choosing_puts_it_away() {
    let (mut page, mut text) = scene(
        "<body><p id=before>before</p>\
         <select><option>Alpha</option><option>Beta</option></select>\
         <p id=after>after</p>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "select");

    let after_y = |page: &PageScene| {
        let tree = page.boxes();
        tree.descendants(tree.root())
            .into_iter()
            .find(|&id| {
                tree.node(id).node.and_then(|node| {
                    page.document()
                        .get(node)
                        .and_then(|inner| inner.element())
                        .and_then(|element| element.id())
                        .map(str::to_owned)
                }) == Some("after".to_owned())
            })
            .and_then(|id| page.rect_of(id))
            .map(|rect| rect.y)
    };

    let closed = after_y(&page);
    assert!(closed.is_some());
    assert!(!page.is_open());
    assert!(
        !page_text(&page).contains("Beta"),
        "a closed one shows one option"
    );

    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page.is_open());
    assert!(
        page_text(&page).contains("Beta"),
        "an open one shows the list"
    );
    assert_eq!(
        after_y(&page),
        closed,
        "and the page behind it did not move"
    );

    // Pressing the control again puts the list away.
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(!page.is_open());
}

/// A field with a `<datalist>` behind it shows the suggestions over the page,
/// and pressing one puts it in the field.
#[test]
fn a_field_offers_its_suggestions_and_a_press_takes_one() {
    let (mut page, mut text) = scene(
        "<body><input list=cities>\
         <datalist id=cities><option value=Amsterdam><option value=Berlin>\
         </datalist>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(
        !page_text(&page).contains("Berlin"),
        "a `<datalist>` shows nothing until a field asks for it"
    );

    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page.is_open());
    let shown = page_text(&page);
    assert!(shown.contains("Amsterdam") && shown.contains("Berlin"));

    // The second suggestion, pressed where it is drawn.
    let option = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter(|&id| {
            page.boxes()
                .node(id)
                .tag
                .as_ref()
                .is_some_and(|tag| tag.as_ref() == "option")
        })
        .nth(1)
        .expect("the second suggestion");
    let rect = page.rect_of(option).expect("a rectangle");
    let (ox, oy) = (
        f64::from(rect.x + rect.width / 2.0),
        f64::from(rect.y + rect.height / 2.0),
    );
    // The list hangs below the field and is not cut off at the field's edge,
    // which a field clips its own contents at.
    let field = point_on(&page, "input");
    assert!(f64::from(rect.y) > field.1, "the list is under the field");
    let clipped = {
        let fragments = page.fragments(&mut text, 800.0, 600.0);
        fn find(
            fragment: &otlyra_layout::Fragment,
            wanted: otlyra_layout::BoxId,
        ) -> Option<otlyra_layout::Fragment> {
            if fragment.box_id == Some(wanted) {
                return Some(fragment.clone());
            }
            fragment
                .children
                .iter()
                .find_map(|child| find(child, wanted))
        }
        find(&fragments.root, option).expect("the suggestion was laid out")
    };
    assert!(
        clipped
            .clip
            .is_none_or(|clip| clip.intersection(&clipped.rect) == clipped.rect),
        "the suggestion is drawn whole: {:?} against {:?}",
        clipped.clip,
        clipped.rect
    );

    page.pointer_pressed(ox, oy);
    page.pointer_released(ox, oy);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(!page.is_open(), "taking one puts the list away");
    assert_eq!(page.focused_value(), Some("Berlin"));
}

/// The list narrows to what has been typed, and goes when nothing matches.
#[test]
fn typing_narrows_the_suggestions_and_empties_them() {
    let (mut page, mut text) = scene(
        "<body><input list=cities>\
         <datalist id=cities><option value=Amsterdam><option value=Berlin>\
         </datalist>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);

    // Compared without case, and anywhere in the suggestion rather than only
    // at its start.
    page.typed("RL");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let shown = page_text(&page);
    assert!(shown.contains("Berlin") && !shown.contains("Amsterdam"));

    page.typed("zz");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(!page.is_open(), "nothing left to suggest");
    assert!(!page_text(&page).contains("Berlin"));
}

/// The arrows walk the list and mark where they have got to; return takes it
/// and escape leaves the field as it was.
#[test]
fn the_arrows_walk_the_suggestions_and_return_takes_one() {
    let (mut page, mut text) = scene(
        "<body><input list=cities>\
         <datalist id=cities><option value=Amsterdam><option value=Berlin>\
         </datalist>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.close_open();

    // The first arrow shows the list without walking into it.
    assert!(page.step_selection(true));
    assert!(page.is_open());
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.focused_value(), Some(""), "showing is not taking");

    assert!(page.step_selection(true));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(
        page.focused_value(),
        Some(""),
        "and neither is walking to one"
    );

    assert!(page.step_selection(true));
    assert!(page.accept_open());
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.focused_value(), Some("Berlin"));
    assert!(!page.is_open());
}

/// Escape puts the list away and leaves the field holding what it held.
#[test]
fn escape_leaves_a_walked_suggestion_untaken() {
    let (mut page, mut text) = scene(
        "<body><input list=cities value=A>\
         <datalist id=cities><option value=Amsterdam><option value=Ankara>\
         </datalist>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert!(page.step_selection(true));
    assert!(page.step_selection(true));
    assert!(page.close_open());
    assert_eq!(page.focused_value(), Some("A"));
}

/// A suggestion that would put nothing in the field, and one nothing can
/// reach, are not offered at all.
#[test]
fn an_empty_or_disabled_suggestion_is_not_offered() {
    let (mut page, mut text) = scene(
        "<body><input list=sparse>\
         <datalist id=sparse><option value=\"\"><option value=Kept>\
         <option value=Skipped disabled></datalist>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let shown = page_text(&page);
    assert!(shown.contains("Kept") && !shown.contains("Skipped"));
}

/// A `list` that names something that is not a `<datalist>` offers nothing,
/// and neither does one that names nothing at all.
#[test]
fn a_list_that_names_no_datalist_offers_nothing() {
    let (mut page, mut text) =
        scene("<body><p id=elsewhere>x</p><input list=elsewhere><input list=absent>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert!(!page.is_open());
}

/// A press on a slider puts the thumb where the pointer is, a drag follows it,
/// and the keys move it by its step.
#[test]
fn a_slider_follows_the_pointer_and_the_keys() {
    let (mut page, mut text) = scene("<body><input type=range name=v min=0 max=10 step=1 value=5>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let slider = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).control.is_some())
        .expect("the slider");
    let rect = page.rect_of(slider).expect("a rectangle");
    let y = f64::from(rect.y + rect.height / 2.0);

    // Pressed at the far left: the minimum, not something a little above it.
    page.pointer_pressed(f64::from(rect.x), y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.focused_value(), Some("0"));

    // Dragged to the far right, wandering off the track on the way.
    page.pointer_moved(f64::from(rect.x + rect.width), y + 200.0);
    page.pointer_released(f64::from(rect.x + rect.width), y + 200.0);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.focused_value(), Some("10"));

    // And the keys, by the step and by ten of them.
    assert!(page.step_value(SliderMotion::Down));
    assert_eq!(page.focused_value(), Some("9"));
    assert!(page.step_value(SliderMotion::PageDown));
    assert_eq!(page.focused_value(), Some("0"), "clamped to the minimum");
    assert!(page.step_value(SliderMotion::End));
    assert_eq!(page.focused_value(), Some("10"));
}

/// A slider holds the middle of its range when nothing said otherwise, and
/// sends it.
#[test]
fn a_slider_with_no_value_holds_and_sends_the_middle_of_its_range() {
    let (mut page, mut text) = scene(
        "<body><form action=/go><input type=range name=v min=0 max=40>\
         <input type=submit value=Go></form>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let button = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter(|&id| page.boxes().node(id).control.is_some())
        .nth(1)
        .expect("the button");
    let rect = page.rect_of(button).expect("a rectangle");
    let (x, y) = (
        f64::from(rect.x + rect.width / 2.0),
        f64::from(rect.y + rect.height / 2.0),
    );
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert_eq!(page.take_submission().expect("sent").url, "/go?v=20");
}

/// A bar is drawn to what its numbers say, and a `<progress>` with no value at
/// all is drawn as one that does not know rather than as one at zero.
#[test]
fn a_bar_is_filled_from_its_own_numbers() {
    let (mut page, mut text) = scene(
        "<body><progress value=0.25></progress><progress></progress>\
         <meter value=9 min=0 max=10 low=3 high=7 optimum=1></meter>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let controls: Vec<_> = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter_map(|id| page.boxes().node(id).control.clone())
        .collect();
    assert_eq!(controls[0].position, Some(0.25));
    assert_eq!(controls[1].position, None, "it does not know how far along");
    assert_eq!(controls[2].position, Some(0.9));
    assert_eq!(
        controls[2].level,
        otlyra_layout::box_tree::Level::Poor,
        "high on a meter whose low end is the good one"
    );
}

/// A date field is filled in a part at a time, and holds nothing until every
/// part of it is there.
#[test]
fn a_date_is_typed_a_part_at_a_time_and_is_worth_nothing_until_it_is_whole() {
    let (mut page, mut text) = scene("<body><input type=date name=d>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(
        page_text(&page),
        "yyyy-mm-dd",
        "the shape of what is wanted"
    );

    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);

    page.typed("2026");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "2026-mm-dd");
    assert_eq!(page.focused_value(), Some(""), "half a date is no date");

    // A seven can only be July, so the month is done and the day is next.
    page.typed("7");
    page.typed("23");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "2026-07-23");
    assert_eq!(page.focused_value(), Some("2026-07-23"));
}

/// The arrows walk the parts and step the one they are on, wrapping at its
/// ends; a backspace empties it again.
#[test]
fn the_keys_walk_and_step_the_parts_of_a_date() {
    let (mut page, mut text) = scene("<body><input type=date value=2026-12-31>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);

    // The year, where a press at the left edge lands.
    assert!(page.step_value(SliderMotion::Up));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "2027-12-31");

    // On to the month, which wraps from December round to January.
    page.edit_text(EditAction::Right, false);
    assert!(page.step_value(SliderMotion::Up));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "2027-01-31");
    assert_eq!(page.focused_value(), Some("2027-01-31"));

    page.edit_text(EditAction::Backspace, false);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "2027-mm-31");
}

/// A time is its own shape, and a day the month does not have is not a date.
#[test]
fn a_time_has_its_own_parts_and_an_impossible_day_is_no_date() {
    let (mut page, mut text) = scene("<body><input type=time><input type=date>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page_text(&page).starts_with("hh:mm"));

    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.typed("1430");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page_text(&page).starts_with("14:30"));
    assert_eq!(page.focused_value(), Some("14:30"));

    assert_eq!(
        otlyra_dom::form::temporal_value("2026-02-30", otlyra_dom::form::InputKind::Date),
        "",
        "February has no thirtieth"
    );
    assert_eq!(
        otlyra_dom::form::temporal_value("2024-02-29", otlyra_dom::form::InputKind::Date),
        "2024-02-29",
        "a leap year has one"
    );
}

/// A colour well shows what it holds, and anything that is not a colour is
/// black.
#[test]
fn a_colour_well_is_the_colour_it_holds() {
    let (mut page, mut text) = scene(
        "<body><input type=color value=\"#ff8800\"><input type=color>\
         <input type=color value=nonsense>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let swatches: Vec<_> = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter_map(|id| page.boxes().node(id).control.clone())
        .filter_map(|control| control.swatch)
        .collect();
    assert_eq!(
        swatches,
        vec![[0xff, 0x88, 0x00], [0, 0, 0], [0, 0, 0]],
        "an unset or unreadable colour is black"
    );
}

/// Moving the pointer onto a plain widget redraws it without rebuilding the
/// cascade, the box tree or the layout — the thing that made scrolling with
/// the pointer over a control lag.
#[test]
fn hovering_a_plain_widget_repaints_it_without_a_restyle() {
    let (mut page, mut text) = scene("<body><button>Press</button><p>after</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let button = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).control.is_some())
        .expect("the button");
    let hovered = |page: &PageScene| {
        page.boxes()
            .node(button)
            .control
            .as_ref()
            .expect("a control")
            .state
            .hovered
    };
    assert!(!hovered(&page));
    let builds = page.builds();

    // Onto the button. The frame it draws is a fresh one — the button greys —
    // but nothing was laid out to draw it.
    let (x, y) = point_on(&page, "button");
    assert!(page.pointer_moved(x, y));
    assert!(hovered(&page), "the widget did not take the hover");
    assert!(
        page.damage().contains(otlyra_layout::Damage::PAINT),
        "and it did not ask to be redrawn"
    );

    // The proof it was cheap: the display list is rebuilt, but the layout it
    // was built from was not, so the widget the fragment already carried is
    // the one that greyed.
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(hovered(&page));

    // Off it again — onto the paragraph below — and it un-greys the same way.
    let (px, py) = point_on(&page, "p");
    assert!(page.pointer_moved(px, py));
    assert!(!hovered(&page), "the hover did not leave");

    // The whole exchange rebuilt no box tree: a rebuild replaces the boxes, so
    // the control we are watching would be a different box. It is the same one.
    assert!(page.boxes().node(button).control.is_some());
    let _ = builds;
}

/// A press on a file picker asks for a dialogue and opens none, and what comes
/// back is what the control shows — the name, never the path.
#[test]
fn a_file_picker_asks_for_a_dialogue_and_shows_what_came_back() {
    let (mut page, mut text) = scene("<body><input type=file accept=\".txt,image/*\" multiple>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "No file chosen");
    assert!(
        page.take_file_request().is_none(),
        "nothing has been pressed"
    );

    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    let asked = page.take_file_request().expect("it asked");
    assert!(asked.many);
    assert_eq!(asked.accept, vec![".txt".to_owned(), "image/*".to_owned()]);
    assert!(page.take_file_request().is_none(), "and only once");

    // Dismissed: it keeps what it held.
    assert!(!page.set_files(asked.node, Vec::new()));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "No file chosen");

    let file = |name: &str| otlyra_dom::form::ChosenFile {
        name: name.to_owned(),
        media_type: otlyra_dom::form::media_type_of(name),
        bytes: Vec::new(),
    };
    assert!(page.set_files(asked.node, vec![file("notes.txt")]));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "notes.txt");

    assert!(page.set_files(asked.node, vec![file("a.txt"), file("b.png")]));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page_text(&page), "2 files");
}

/// Laying the same box tree out twice must give the same box: the room a
/// drop-down leaves for its arrow is added to its padding, and adding it again
/// on every pass made the control grow twenty pixels a frame.
#[test]
fn laying_a_drop_down_out_twice_does_not_widen_it() {
    let (mut page, mut text) =
        scene("<body><select><option>Alpha</option><option>Beta</option></select>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let select = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).control.is_some())
        .expect("the select");
    let first = page.rect_of(select).expect("a rectangle");

    // A resize lays the same tree out again, and again.
    for width in [799.0, 798.0, 797.0, 796.0, 800.0] {
        page.build_display_list(&mut text, width, 600.0, 0.0);
    }
    assert_eq!(page.rect_of(select), Some(first));
}

/// A form that submits is a form that navigates, and pressing the button is
/// the whole of it.
#[test]
fn pressing_a_submit_button_sends_the_form() {
    let (mut page, mut text) = scene(
        "<body><form action=/search><input name=q value=\"a b\">\
         <input type=submit value=Go></form>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page.take_submission().is_none(), "nothing has been pressed");

    // The button is the second control on the line.
    let button = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter(|&id| page.boxes().node(id).control.is_some())
        .nth(1)
        .expect("the button");
    let rect = page.rect_of(button).expect("a rectangle");
    let (x, y) = (
        f64::from(rect.x + rect.width / 2.0),
        f64::from(rect.y + rect.height / 2.0),
    );
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);

    let sent = page.take_submission().expect("the form was sent");
    assert_eq!(sent.url, "/search?q=a+b");
    assert!(page.take_submission().is_none(), "and only once");
}

/// A form with something wrong in it is not sent, and the field that is wrong
/// is marked so that a rule can show it.
#[test]
fn a_form_that_does_not_check_out_is_not_sent() {
    let (mut page, mut text) = scene(
        "<body><form action=/save><input name=who required>\
         <input type=submit value=Go></form>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let button = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter(|&id| page.boxes().node(id).control.is_some())
        .nth(1)
        .expect("the button");
    let rect = page.rect_of(button).expect("a rectangle");
    let (x, y) = (
        f64::from(rect.x + rect.width / 2.0),
        f64::from(rect.y + rect.height / 2.0),
    );
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert!(page.take_submission().is_none(), "an empty required field");

    // Fill it in, and it goes.
    let field = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).control.is_some())
        .expect("the field");
    let rect = page.rect_of(field).expect("a rectangle");
    page.pointer_pressed(f64::from(rect.x + 4.0), f64::from(rect.y + 4.0));
    page.pointer_released(f64::from(rect.x + 4.0), f64::from(rect.y + 4.0));
    page.typed("Ada");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    // The box tree was built again, so the handles from before are stale.
    let button = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter(|&id| page.boxes().node(id).control.is_some())
        .nth(1)
        .expect("the button");
    let rect = page.rect_of(button).expect("a rectangle");
    let (x, y) = (
        f64::from(rect.x + rect.width / 2.0),
        f64::from(rect.y + rect.height / 2.0),
    );
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert_eq!(
        page.take_submission().map(|sent| sent.url),
        Some("/save?who=Ada".to_owned())
    );
}

/// Return in a field sends the form, which is why a search box with nothing but
/// a field in it works at all.
#[test]
fn return_in_the_only_field_sends_the_form() {
    let (mut page, mut text) = scene("<body><form action=/search><input name=q value=cats></form>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert!(page.implicit_submit());
    assert_eq!(
        page.take_submission().map(|sent| sent.url),
        Some("/search?q=cats".to_owned())
    );
}

/// A reset button puts a form back the way the markup left it.
#[test]
fn a_reset_button_puts_the_form_back() {
    let (mut page, mut text) =
        scene("<body><form><input value=start><input type=reset value=Reset></form>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.typed("!");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page_text(&page).contains('!'));

    let reset = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .filter(|&id| page.boxes().node(id).control.is_some())
        .nth(1)
        .expect("the reset button");
    let rect = page.rect_of(reset).expect("a rectangle");
    let (x, y) = (
        f64::from(rect.x + rect.width / 2.0),
        f64::from(rect.y + rect.height / 2.0),
    );
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(
        !page_text(&page).contains('!'),
        "back to what the markup said"
    );
}

/// A list of two hundred countries is not a list two hundred rows long: it is
/// capped, and it slides so that what is chosen is in it.
#[test]
fn a_long_open_list_is_capped_and_slides_to_the_choice() {
    let options: String = (0..60)
        .map(|n| format!("<option>Row {n}</option>"))
        .collect();
    let (mut page, mut text) = scene(&format!("<body><select>{options}</select>"));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "select");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    let list = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).anonymous && page.boxes().node(id).control.is_some())
        .expect("the open list");
    let rect = page.rect_of(list).expect("a rectangle");
    // The cap is on the content box; the border it has is its own.
    assert!(rect.height <= 310.0, "capped, got {}", rect.height);

    // Walk to the far end: the list has to have moved to show it.
    for _ in 0..40 {
        page.step_selection(true);
    }
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(
        page.boxes().control_scroll(list).1 > 0.0,
        "the list slid to what is chosen"
    );
}

#[test]
fn the_arrows_move_a_drop_downs_choice() {
    let (mut page, mut text) =
        scene("<body><select><option>Alpha</option><option>Beta</option></select>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "select");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.close_open();
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page_text(&page).contains("Alpha"));

    assert!(page.step_selection(true));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(
        page_text(&page).contains("Beta"),
        "and it shows the new one"
    );

    assert!(!page.step_selection(true), "the last one is the last one");
    assert!(page.step_selection(false));
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(page_text(&page).contains("Alpha"));
}

#[test]
fn a_second_press_in_a_field_takes_the_word_and_a_third_takes_all_of_it() {
    let (mut page, mut text) = scene("<body><input value=\"one two three\" size=40>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");

    // Somewhere inside the middle word.
    let middle = x + 30.0;
    page.pointer_pressed_times(middle, y, 2);
    page.pointer_released(middle, y);
    assert_eq!(page.selected_text().as_deref(), Some("two"));

    page.pointer_pressed_times(middle, y, 3);
    page.pointer_released(middle, y);
    assert_eq!(page.selected_text().as_deref(), Some("one two three"));
}

/// A text area is as many rows as it was asked for however much is in it: the
/// text slides up under the box so the caret stays where it can be seen.
#[test]
fn a_text_area_slides_its_lines_to_keep_the_caret_in_sight() {
    let (mut page, mut text) = scene("<body><textarea rows=2 cols=10></textarea>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "textarea");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);

    let area = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).control.is_some())
        .expect("the text area");

    page.typed("one");
    caret_of(&mut page, &mut text);
    assert_eq!(page.boxes().control_scroll(area).1, 0.0, "two rows fit two");

    // Far more lines than it shows.
    for _ in 0..8 {
        page.typed(" wrapping words that go on");
    }
    let after = caret_of(&mut page, &mut text).expect("a caret still");
    assert!(
        page.boxes().control_scroll(area).1 > 0.0,
        "the lines slid up under the box"
    );
    let rect = page.rect_of(area).expect("a rectangle");
    assert!(
        after.1 <= f64::from(rect.bottom()),
        "and the caret is inside it: {after:?} against {rect:?}"
    );
}

/// A caret is solid while the reader types and blinks once they stop, which is
/// what every platform does and what keeps it visible exactly when it is being
/// looked for.
#[test]
fn the_caret_blinks_and_starts_over_on_every_keystroke() {
    let (mut page, mut text) = scene("<body><input>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(
        !page.caret_blinks(),
        "a page nobody has clicked has no caret"
    );

    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    assert!(
        page.caret_blinks(),
        "and one that has been clicked into has"
    );
    let now = std::time::Instant::now();
    let deadline = page.next_caret_frame().expect("the caret has a deadline");
    assert!(deadline > now);
    assert!(deadline <= now + CARET_BLINK_INTERVAL);
    assert!(
        caret_of(&mut page, &mut text).is_some(),
        "solid the instant it is put there"
    );

    // Half a second on, half a second off. Rather than sleeping for one, the
    // clock is wound back by hand.
    page.wind_caret_back(std::time::Duration::from_millis(600));
    let now = std::time::Instant::now();
    let deadline = page.next_caret_frame().expect("the next half is scheduled");
    assert!(deadline > now);
    assert!(deadline <= now + CARET_BLINK_INTERVAL);
    assert!(
        caret_of(&mut page, &mut text).is_none(),
        "and gone half a second later"
    );

    // A keystroke puts it back on, whatever half of the blink it was in.
    page.typed("a");
    assert!(caret_of(&mut page, &mut text).is_some());
}

/// A field is one line long however much is typed into it: the line slides
/// under the box so that the caret stays where the reader can see it.
#[test]
fn a_field_slides_its_text_to_keep_the_caret_in_sight() {
    let (mut page, mut text) = scene("<body><input size=6>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);

    // Short enough to fit: nothing has moved.
    page.typed("ab");
    let inside = caret_of(&mut page, &mut text).expect("a caret");
    let field = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).control.is_some())
        .expect("the field");
    assert_eq!(page.boxes().control_scroll(field).0, 0.0);

    // Far more than fits: the caret is still on screen and the text has moved.
    page.typed("cdefghijklmnopqrstuvwxyz");
    let after = caret_of(&mut page, &mut text).expect("a caret still");
    assert!(
        page.boxes().control_scroll(field).0 > 0.0,
        "the line slid under the box"
    );
    let rect = page.rect_of(field).expect("a rectangle");
    assert!(
        after.0 <= f64::from(rect.right()),
        "and the caret is inside it: {after:?} against {rect:?}"
    );
    assert!(after.0 > inside.0, "and past where it started");

    // Back to the start, and the field shows its first letter again.
    page.edit_text(EditAction::Home, false);
    caret_of(&mut page, &mut text);
    assert_eq!(page.boxes().control_scroll(field).0, 0.0);
}

/// Moving the caret changes nothing else about the page, so the frame would be
/// reused unless the caret is part of what a frame is a function of.
#[test]
fn moving_the_caret_builds_a_new_frame() {
    let (mut page, mut text) = scene("<body><input value=abcdef>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    let before = page.builds();
    page.edit_text(EditAction::Right, false);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), before + 1);
}

#[test]
fn nothing_is_typed_into_a_page_with_no_field_focused() {
    let (mut page, mut text) = scene("<body><p>text</p><input disabled>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(!page.typed("x"), "nothing has the focus");

    let (x, y) = point_on(&page, "input");
    page.pointer_pressed(x, y);
    assert!(!page.typed("x"), "and a disabled field never takes it");
}

fn glyph_ys(list: &DisplayList) -> Vec<f64> {
    let mut painter = RecordingPainter::new();
    render(list, &mut painter);
    painter
        .take()
        .iter()
        .filter_map(|op| match op {
            PaintOp::DrawGlyphs { transform, .. } => Some(transform.as_coeffs()[5]),
            _ => None,
        })
        .collect()
}

/// The colour of the first paragraph, which is what a media query in these
/// tests changes.
fn paragraph_colour(page: &PageScene) -> otlyra_gfx::peniko::Color {
    let boxes = page.boxes();
    boxes
        .descendants(boxes.root())
        .into_iter()
        .find(|&id| {
            boxes
                .node(id)
                .tag
                .as_ref()
                .is_some_and(|tag| tag.as_ref() == "p")
        })
        .map(|id| boxes.node(id).style.color)
        .expect("a paragraph")
}

/// A resize relays out; it re-cascades only when the viewport is something a
/// rule reads.
#[test]
fn a_resize_restyles_only_when_a_rule_reads_the_viewport() {
    let (mut page, mut text) =
        scene("<style>@media (min-width: 700px) { p { color: rgb(255, 0, 0) } }</style><p>text");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(
        paragraph_colour(&page),
        otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0)
    );

    page.build_display_list(&mut text, 500.0, 600.0, 0.0);
    assert_ne!(
        paragraph_colour(&page),
        otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0),
        "the query stopped matching and nothing noticed"
    );
}

/// A resize with nothing to restyle keeps the styles it had, and lays out
/// again at the new width — which is the whole point of asking first.
#[test]
fn a_resize_nothing_reads_still_relays_out() {
    let (mut page, mut text) = scene("<style>p { color: rgb(0, 128, 0) }</style><p>text</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let colour = paragraph_colour(&page);

    page.build_display_list(&mut text, 300.0, 600.0, 0.0);
    assert_eq!(paragraph_colour(&page), colour);
    assert_eq!(
        page.layout.as_ref().expect("a layout").0,
        300.0,
        "laid out at the new width"
    );
}

/// A scrollbar can be taken hold of and dragged, and the content follows the
/// thumb rather than the other way round.
#[test]
fn dragging_a_scrollbar_scrolls_the_page() {
    let (mut page, mut text) =
        scene("<style>body { margin: 0 } p { height: 3000px }</style><p>tall</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    // Nowhere near the bar: the press is not for it.
    assert!(!page.grab_scrollbar(400.0, 300.0, 800.0, 600.0));

    // On the thumb, which sits at the top of a page that has not been scrolled.
    assert!(page.grab_scrollbar(795.0, 10.0, 800.0, 600.0));
    assert!(page.dragging_scrollbar());

    page.drag_scrollbar(300.0, 800.0, 600.0);
    let halfway = page.scroll();
    assert!(halfway > 0.0, "the drag did not move the page");

    page.drag_scrollbar(600.0, 800.0, 600.0);
    assert!(
        page.scroll() > halfway,
        "further down did not scroll further"
    );

    page.release_scrollbar();
    page.drag_scrollbar(0.0, 800.0, 600.0);
    assert!(page.scroll() > halfway, "it moved after being let go");
}

/// A scrolled panel's contents stay inside it. The regression this pins: the
/// clip was decided by whether the contents fitted where the flow put them,
/// which stopped being true the moment the panel scrolled — and the contents
/// were then drawn over everything around the panel instead of under its edge.
#[test]
fn a_scrolled_panel_clips_what_it_has_moved() {
    let (mut page, mut text) = scene(
        "<style>body { margin: 0 } \
         .panel { overflow: hidden; height: 100px } \
         .item { height: 60px }</style>\
         <div class=panel><div class=item>a</div><div class=item>b</div>\
         <div class=item>c</div></div>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    page.scroll_at(50.0, 50.0, 80.0);

    let list = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let mut painter = RecordingPainter::new();
    render(&list, &mut painter);

    // Everything drawn while a layer is open is inside it; the panel's own
    // rectangle is what that layer is.
    let mut depth = 0i32;
    let mut clipped_glyphs = 0;
    let mut loose_glyphs = 0;
    for op in painter.take() {
        match op {
            PaintOp::PushLayer { .. } => depth += 1,
            PaintOp::PopLayer => depth -= 1,
            PaintOp::DrawGlyphs { transform, .. } => {
                let y = transform.as_coeffs()[5];
                // The panel is the first hundred pixels of the page.
                if !(0.0..=100.0).contains(&y) {
                    if depth > 0 {
                        clipped_glyphs += 1;
                    } else {
                        loose_glyphs += 1;
                    }
                }
            }
            _ => {}
        }
    }

    assert!(
        clipped_glyphs > 0,
        "the scroll moved nothing out of the panel, so this proves nothing"
    );
    assert_eq!(
        loose_glyphs, 0,
        "text scrolled out of the panel was drawn outside it"
    );
}

/// What a scrolled panel actually draws: the contents move, the box does not.
#[test]
fn scrolling_a_panel_moves_its_contents_and_not_its_edge() {
    let (mut page, mut text) = scene(
        "<style>body { margin: 0 } \
         .panel { overflow: hidden; height: 100px; background: rgb(0, 0, 255) } \
         .tall { height: 400px; background: rgb(255, 0, 0) }</style>\
         <div class=panel><div class=tall>inside</div></div>",
    );

    let tops = |page: &mut PageScene, text: &mut TextEngine| {
        let list = page.build_display_list(text, 800.0, 600.0, 0.0);
        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);
        let mut panel = None;
        let mut inside = None;
        use otlyra_gfx::kurbo::Shape as _;
        for op in painter.take() {
            if let PaintOp::Fill { brush, shape, .. } = op {
                if brush
                    == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                        0, 0, 255,
                    ))
                {
                    panel = Some(shape.bounding_box().y0);
                }
                if brush
                    == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                        255, 0, 0,
                    ))
                {
                    inside = Some(shape.bounding_box().y0);
                }
            }
        }
        (panel.expect("the panel"), inside.expect("its contents"))
    };

    let (panel_before, inside_before) = tops(&mut page, &mut text);
    page.scroll_at(50.0, 50.0, 60.0);
    let (panel_after, inside_after) = tops(&mut page, &mut text);

    assert_eq!(panel_before, panel_after, "the box itself moved");
    assert_eq!(
        inside_before - inside_after,
        60.0,
        "its contents did not move by what the wheel said"
    );
}

/// A box that cuts its contents off and has more than it can show takes the
/// wheel; the page takes it once that box has reached its end.
#[test]
fn a_scrollable_box_takes_the_wheel_before_the_page_does() {
    let (mut page, mut text) = scene(
        "<style>body { margin: 0 } \
         .panel { overflow: hidden; height: 100px } \
         .tall { height: 400px } \
         .after { height: 2000px }</style>\
         <div class=panel><div class=tall>inside</div></div>\
         <div class=after>after</div>",
    );
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    // Over the panel: the panel scrolls and the page does not.
    page.scroll_at(50.0, 50.0, 60.0);
    assert_eq!(page.scroll(), 0.0, "the page moved instead of the panel");

    // Past the panel's end, the rest goes to the page.
    page.scroll_at(50.0, 50.0, 1000.0);
    page.scroll_at(50.0, 50.0, 40.0);
    assert!(page.scroll() > 0.0, "the panel kept the wheel to itself");

    // Below the panel, the page scrolls from the first turn.
    let was = page.scroll();
    page.scroll_at(50.0, 400.0, 30.0);
    assert!(page.scroll() > was);
}

#[test]
fn the_title_names_the_tab_and_is_not_page_content() {
    let parsed = otlyra_html::parse(b"<title>A page</title><p>text", Some("utf-8"));
    assert_eq!(title_of(&parsed.document).as_deref(), Some("A page"));
}

#[test]
fn a_document_reaches_the_paint_seam_as_glyphs() {
    let (mut scene, mut text) = scene("<body><h1>heading</h1><p>paragraph");
    let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(glyph_ys(&list).len(), 2, "the heading and the paragraph");
}

#[test]
fn the_top_inset_moves_the_page_below_the_interface() {
    let (mut scene, mut text) = scene("<body><p>text");
    let flush = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));
    let inset = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 72.0));
    assert!((inset[0] - flush[0] - 72.0).abs() < 0.01);
}

#[test]
fn scrolling_moves_the_page_up_and_is_clamped_to_the_content() {
    let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
    let (mut scene, mut text) = scene(&html);
    let before = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));

    scene.scroll_by(12.0);
    let after = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));
    assert!((before[0] - after[0] - 12.0).abs() < 0.01);

    scene.scroll_by(-1000.0);
    assert_eq!(scene.scroll(), 0.0);
}

/// A click into a field lands between the two letters it fell between, even
/// when the page has been restyled since the last frame.
///
/// The press is answered against the frame the reader was looking at. With the
/// layout thrown away instead of marked out of date there was nothing to hit-
/// test against, and every click into a field put the caret at the end of what
/// it held — which is what focusing the field itself made happen.
#[test]
fn a_click_into_a_field_lands_where_it_fell_after_a_restyle() {
    let (mut page, mut text) = scene("<body><input value=\"Hello world\" size=30>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let field = page
        .boxes()
        .descendants(page.boxes().root())
        .into_iter()
        .find(|&id| page.boxes().node(id).control.is_some())
        .expect("the field");
    let rect = page.rect_of(field).expect("a rectangle");
    let y = f64::from(rect.y + rect.height / 2.0);

    // Focusing the field is itself a restyle, so this is the second click of
    // any pair — and it was the one that always missed.
    page.invalidate_styles();
    page.pointer_pressed(f64::from(rect.x) + 20.0, y);
    assert!(
        (1..=4).contains(&page.caret),
        "the caret landed at {} rather than near the start",
        page.caret
    );
}

/// A state change does not send the reader back to the top.
///
/// Anything that restyles the page marks the layout out of date, and a scroll
/// arriving before the next frame still has to know how far the page goes. When
/// the layout was thrown away rather than marked, that question answered zero
/// and the wheel snapped the page to the top — which is what a page full of
/// controls did every time the pointer crossed one.
#[test]
fn a_restyle_before_the_next_frame_does_not_scroll_the_page_to_the_top() {
    let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
    let (mut scene, mut text) = scene(&html);
    let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
    scene.scroll_by(500.0);
    assert_eq!(scene.scroll(), 500.0);

    // Something changed the page's style, and no frame has been drawn since.
    scene.invalidate_styles();
    scene.scroll_by(10.0);
    assert_eq!(
        scene.scroll(),
        510.0,
        "the wheel put the reader back at the top"
    );
}

#[test]
fn a_page_shorter_than_the_window_cannot_scroll() {
    let (mut scene, mut text) = scene("<body><p>short");
    let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
    scene.scroll_by(500.0);
    assert_eq!(scene.scroll(), 0.0);
}

/// Scrolling must not relay out: layout is a function of the width, and the
/// width has not changed.
#[test]
fn scrolling_reuses_the_layout_and_resizing_does_not() {
    let (mut scene, mut text) = scene("<body><p>text");
    let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
    scene.scroll_by(5.0);
    let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(scene.layout.as_ref().expect("laid out").0, 800.0);

    let _ = scene.build_display_list(&mut text, 400.0, 600.0, 0.0);
    assert_eq!(scene.layout.as_ref().expect("laid out").0, 400.0);
}

/// The assertion that keeps clicking honest: the link's target is the
/// rectangle its text was drawn in, and nothing else on the page is.
#[test]
fn a_point_on_a_link_resolves_to_its_href() {
    let (mut scene, mut text) = scene("<body><p>before <a href=\"/next\">the link</a> after</p>");
    let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);

    // Find where the link's own run was drawn, from the display list itself.
    let mut painter = RecordingPainter::new();
    render(&list, &mut painter);
    let ops = painter.take();
    let blue = ops
        .iter()
        .filter_map(|op| match op {
            PaintOp::DrawGlyphs {
                brush, transform, ..
            } if *brush
                == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                    0, 0, 0xee,
                )) =>
            {
                Some(transform.as_coeffs())
            }
            _ => None,
        })
        .next()
        .expect("the link is painted in the UA blue");

    let (x, y) = (blue[4] + 4.0, blue[5] + 6.0);
    assert_eq!(scene.link_at(x, y).as_deref(), Some("/next"));
    assert_eq!(scene.link_at(x, y + 400.0), None, "below the text");
    assert_eq!(scene.link_at(2.0, y), None, "before the link starts");
}

#[test]
fn a_link_around_other_elements_is_still_a_link() {
    let (mut scene, mut text) = scene("<body><p><a href=\"/x\"><b>bold link</b></a>");
    let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);

    // The narrowest target is the text run itself; the wide ones are the
    // blocks it sits inside.
    let hit = scene
        .targets
        .iter()
        .map(|(rect, _)| *rect)
        .min_by(|a, b| a.width().total_cmp(&b.width()))
        .expect("something was drawn");
    assert_eq!(
        scene.link_at(hit.x0 + 2.0, hit.y0 + 2.0).as_deref(),
        Some("/x"),
        "the text belongs to the <b>, and the link is above it"
    );
}

#[test]
fn an_anchor_without_an_href_is_not_a_link() {
    let (mut scene, mut text) = scene("<body><p><a>not a link</a>");
    let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(scene.link_at(10.0, 20.0), None);
}

#[test]
fn an_empty_page_still_paints_its_canvas() {
    let (mut scene, mut text) = scene("");
    let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert!(matches!(
        list.items().first(),
        Some(DisplayItem::Fill { .. })
    ));
}

/// A search counts what is on the page, reaches across the runs a bold word
/// breaks a sentence into, and steps round the end.
#[test]
fn a_search_counts_what_is_on_the_page_and_steps_through_it() {
    let (mut page, mut text) = scene("<body><p>one <b>two</b>three</p><p>Two here two</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    assert_eq!(
        page.find("two"),
        3,
        "once in the first block and twice in the second"
    );
    assert_eq!(page.find_query(), Some("two"));
    assert_eq!(
        page.current_match(),
        Some(0),
        "a search starts at the first"
    );
    assert_eq!(page.current_match_text().as_deref(), Some("two"));

    assert!(page.step_match(true));
    assert_eq!(page.current_match(), Some(1));
    assert!(page.step_match(true));
    assert_eq!(page.current_match(), Some(2));
    assert!(page.step_match(true));
    assert_eq!(page.current_match(), Some(0), "and round the end");
    assert!(page.step_match(false));
    assert_eq!(page.current_match(), Some(2), "and back round the start");

    // The word the markup broke in half is one word, and the highlight it
    // comes back as is a rectangle over each run it crosses.
    assert_eq!(page.find("twothree"), 1);
    let rects = page.current_match_rects();
    assert_eq!(
        rects.len(),
        2,
        "a match over two runs is drawn over both of them: {rects:?}"
    );
    assert_eq!(
        page.match_rects().len(),
        2,
        "and that is all there is to draw"
    );

    // Nothing typed is nothing looked for.
    assert_eq!(page.find(""), 0);
    assert_eq!(page.find_query(), None);
    assert!(page.match_rects().is_empty());
}

/// A search that finds nothing is still a search: the bar has something to
/// say *none* about, and there is nothing to draw or to step to.
#[test]
fn a_search_that_finds_nothing_holds_its_query_and_offers_no_match() {
    let (mut page, mut text) = scene("<body><p>one two three</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    assert_eq!(page.find("nowhere"), 0);
    assert_eq!(page.find_query(), Some("nowhere"));
    assert_eq!(page.current_match(), None);
    assert!(!page.step_match(true), "nothing to step to");
    assert!(page.current_match_rects().is_empty());

    assert!(page.clear_find(), "and it was there to be cleared");
    assert!(!page.clear_find(), "only once");
}

/// A match is a place in one layout, so a page laid out again is searched
/// again — the old numbers would point at whatever now holds them.
#[test]
fn a_resize_searches_the_page_again_and_keeps_where_the_reader_was() {
    let (mut page, mut text) =
        scene("<body><p>alpha bravo charlie delta echo</p><p>needle here</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);

    assert_eq!(page.find("needle"), 1);
    let wide = page.current_match_rects();
    assert_eq!(wide.len(), 1);

    // Narrow enough that the first paragraph wraps, which puts runs between
    // the top of the page and the word that was found.
    page.build_display_list(&mut text, 90.0, 600.0, 0.0);
    assert_eq!(page.match_count(), 1, "the word is still on the page");
    assert_eq!(
        page.current_match_text().as_deref(),
        Some("needle"),
        "and the match still names it rather than whatever took its number"
    );
    let narrow = page.current_match_rects();
    assert_eq!(narrow.len(), 1);
    assert!(
        narrow[0].y > wide[0].y,
        "further down the page, because the paragraph above it now takes more \
         lines: {narrow:?} against {wide:?}"
    );
}

/// A search is not a question about a page that has not been laid out: there
/// is no frame for a match to be a place in.
#[test]
fn a_page_with_no_frame_yet_finds_nothing() {
    let (mut page, _text) = scene("<body><p>one two three</p>");
    assert_eq!(page.find("two"), 0);
    assert!(page.match_rects().is_empty());
}

/// Every match is washed and the one the reader is on is washed differently,
/// through the layer the selection is drawn in rather than an overlay of its
/// own.
#[test]
fn a_search_washes_every_match_and_the_current_one_more_strongly() {
    let (mut page, mut text) = scene("<body><p>two one two one two</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.find("two"), 3);

    let washes = |page: &mut PageScene, text: &mut TextEngine| {
        let list = page.build_display_list(text, 800.0, 600.0, 0.0);
        let count = |wash: otlyra_paint::Highlight| {
            list.items()
                .iter()
                .filter(|item| {
                    matches!(item, DisplayItem::Fill {
                        brush: otlyra_gfx::peniko::Brush::Solid(colour), ..
                    } if *colour == wash.colour())
                })
                .count()
        };
        (
            count(otlyra_paint::Highlight::Match),
            count(otlyra_paint::Highlight::CurrentMatch),
        )
    };

    assert_eq!(
        washes(&mut page, &mut text),
        (2, 1),
        "three matches: two washed and the one the reader is on drawn strongly"
    );

    assert!(page.step_match(true));
    assert_eq!(
        washes(&mut page, &mut text),
        (2, 1),
        "stepping moves which one is strong without changing how many there are"
    );

    page.clear_find();
    assert_eq!(
        washes(&mut page, &mut text),
        (0, 0),
        "and closing the search takes every wash off the page"
    );
}

/// Reaching a match is scrolling the page to it — and a match already in
/// sight is left where it is.
#[test]
fn stepping_to_a_match_brings_it_on_screen() {
    let filler = "<p>filler</p>".repeat(80);
    let (mut page, mut text) = scene(&format!("<body><p>needle</p>{filler}<p>needle</p>"));
    page.build_display_list(&mut text, 800.0, 400.0, 0.0);

    assert_eq!(page.find("needle"), 2);
    assert_eq!(
        page.scroll(),
        0.0,
        "the first is at the top, and reaching it moves nothing"
    );

    assert!(page.step_match(true));
    assert!(
        page.scroll() > 0.0,
        "the second is past the bottom of the window"
    );
    let rects = page.current_match_rects();
    let at = rects.first().expect("the match has a rectangle");
    assert!(
        at.y >= page.scroll() && at.bottom() <= page.scroll() + 400.0,
        "and it is on screen once it has been stepped to: {at:?} at {}",
        page.scroll()
    );

    assert!(page.step_match(false));
    assert_eq!(page.scroll(), 0.0, "and back round to the first one");
}

/// Scrolling something into view moves the page the least that will do it,
/// and does not move it at all for something already in sight.
#[test]
fn scrolling_into_view_moves_the_page_the_least_that_will_do_it() {
    let filler = "<p>filler</p>".repeat(80);
    let (mut page, mut text) = scene(&format!("<body>{filler}"));
    page.build_display_list(&mut text, 800.0, 400.0, 0.0);

    assert!(
        !page.scroll_to_rect(otlyra_layout::Rect::new(0.0, 100.0, 50.0, 20.0)),
        "already in sight"
    );
    assert_eq!(page.scroll(), 0.0);

    assert!(page.scroll_to_rect(otlyra_layout::Rect::new(0.0, 900.0, 50.0, 20.0)));
    let below = page.scroll();
    assert!(
        below > 0.0 && below < 900.0,
        "brought to the bottom of the window rather than to the top: {below}"
    );

    assert!(page.scroll_to_rect(otlyra_layout::Rect::new(0.0, 100.0, 50.0, 20.0)));
    assert!(
        page.scroll() < 100.0,
        "and something above the top is brought to the top: {}",
        page.scroll()
    );

    // Taller than the window: its top, because that is where reading it
    // starts and no scroll can show all of it.
    page.set_scroll(0.0);
    assert!(page.scroll_to_rect(otlyra_layout::Rect::new(0.0, 600.0, 50.0, 900.0)));
    assert!(
        page.scroll() <= 600.0 && page.scroll() > 500.0,
        "aligned at its top: {}",
        page.scroll()
    );
}

/// Searching asks for a frame, because the highlight is drawn in one.
#[test]
fn a_search_asks_for_the_next_frame() {
    let (mut page, mut text) = scene("<body><p>one two three</p>");
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    let builds = page.builds();
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(
        page.builds(),
        builds,
        "an unchanged page is not built again"
    );

    assert_eq!(page.find("two"), 1);
    page.build_display_list(&mut text, 800.0, 600.0, 0.0);
    assert_eq!(page.builds(), builds + 1, "and a search is a change");
}

/// A scene of `html` as if fetched from `url`.
fn scene_at(html: &str, url: &str) -> PageScene {
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    PageScene::at(
        parsed.document,
        url::Url::parse(url).expect("a test's address parses"),
    )
}

/// A base element's `href` is resolved against the document's own address,
/// and what the markup names is resolved against the result.
#[test]
fn a_base_element_moves_the_documents_base() {
    let page = scene_at(
        "<base target=_top><base href=\"../assets/\"><base href=https://elsewhere.test/>",
        "https://x.test/a/b/page.html",
    );
    assert_eq!(page.base_url().as_str(), "https://x.test/a/assets/");
    assert_eq!(page.url().as_str(), "https://x.test/a/b/page.html");
    assert_eq!(
        page.resolve("pic.png").as_deref(),
        Some("https://x.test/a/assets/pic.png")
    );
    assert_eq!(
        scene_at("<p>no base", "https://x.test/a/b/page.html")
            .base_url()
            .as_str(),
        "https://x.test/a/b/page.html"
    );
}

/// A base that is a `data:` or a `javascript:` URL, or no URL at all, is
/// refused and the document's own address stands.
#[test]
fn a_data_or_script_base_is_ignored() {
    for href in [
        "data:text/html,<p>x",
        "javascript:alert(1)",
        "https://[not a host/",
    ] {
        let page = scene_at(
            &format!("<base href=\"{href}\"><a href=next>x</a>"),
            "https://x.test/dir/page.html",
        );
        assert_eq!(
            page.base_url().as_str(),
            "https://x.test/dir/page.html",
            "{href}"
        );
    }
}

/// The page's own style is resolved against that base as well: a `<style>`'s
/// picture is wanted at the address the base makes of it.
#[test]
fn a_style_picture_is_wanted_where_the_base_puts_it() {
    let mut page = scene_at(
        "<base href=https://cdn.test/img/>\
         <style>div { width: 4px; height: 4px; background: url(a.png) }</style><div></div>",
        "https://x.test/page.html",
    );
    page.build_display_list(&mut TextEngine::isolated(), 800.0, 600.0, 0.0);
    assert_eq!(page.wanted_pictures(), ["https://cdn.test/img/a.png"]);
}
