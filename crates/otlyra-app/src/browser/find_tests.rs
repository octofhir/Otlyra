//! Finding a run of characters in the page, from the bar down to the wash.

use otlyra_platform::{Key, Modifiers, Painter, PlatformEvent};

use super::tests::*;
use super::*;

/// The platform's own accelerator modifier, whichever platform this is.
const ACCELERATOR: Modifiers = Modifiers {
    command: cfg!(target_os = "macos"),
    control: !cfg!(target_os = "macos"),
    shift: false,
    alt: false,
};

fn frame(browser: &mut Browser) {
    let mut target = otlyra_gfx::RecordingPainter::default();
    browser.paint(&mut target, Viewport::new(800, 600, 1.0));
}

fn key(browser: &mut Browser, key: Key, modifiers: Modifiers) {
    browser.on_event(PlatformEvent::KeyPressed { key, modifiers });
}

/// Open the bar and type `query` into it, the way a reader does.
fn look_for(browser: &mut Browser, query: &str) {
    frame(browser);
    key(browser, Key::Character('f'), ACCELERATOR);
    frame(browser);
    for character in query.chars() {
        browser.on_event(PlatformEvent::TextInput(character));
    }
}

/// A page holding the same word three times, so stepping has somewhere to go.
fn three_needles() -> Browser {
    let mut browser = browser();
    go(&mut browser, "file:///a/needle/needle/needle");
    browser
}

/// ⌘F, a query, and the page is searched: the count reaches the bar and the
/// wash reaches the page.
#[test]
fn the_bar_searches_the_page_and_steps_through_what_it_found() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");

    assert!(browser.ui.finding());
    assert_eq!(
        browser.ui.find_status,
        crate::ui::FindStatus {
            total: 3,
            current: 1
        },
        "the bar counts what the page found"
    );
    let page = browser.tabs[0].page.as_ref().expect("a loaded page");
    assert_eq!(page.match_count(), 3);
    assert_eq!(
        page.match_rects().len(),
        3,
        "and every one of them has somewhere to be drawn"
    );

    // Return steps on, shift-Return steps back, and both wrap.
    key(&mut browser, Key::Enter, Modifiers::default());
    assert_eq!(browser.ui.find_status.current, 2);
    let shift = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    key(&mut browser, Key::Enter, shift);
    assert_eq!(browser.ui.find_status.current, 1);
    key(&mut browser, Key::Enter, shift);
    assert_eq!(browser.ui.find_status.current, 3, "round the start");

    // Escape closes the bar and takes the wash off the page with it.
    key(&mut browser, Key::Escape, Modifiers::default());
    assert!(!browser.ui.finding());
    let page = browser.tabs[0].page.as_ref().expect("a loaded page");
    assert_eq!(page.match_count(), 0);
    assert!(page.match_rects().is_empty());
}

/// A query nothing on the page holds is still a query: the bar says none
/// rather than saying nothing, and there is nothing to step to.
#[test]
fn a_query_the_page_does_not_hold_counts_none() {
    let mut browser = three_needles();
    look_for(&mut browser, "haystack");

    assert_eq!(
        browser.ui.find_status,
        crate::ui::FindStatus {
            total: 0,
            current: 0
        }
    );
    key(&mut browser, Key::Enter, Modifiers::default());
    assert_eq!(browser.ui.find_status.current, 0, "nowhere to step to");
}

/// ⌘G steps without the bar holding the keyboard, which is what lets a
/// reader look at the page they searched.
#[test]
fn command_g_steps_while_the_page_has_the_keyboard() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");
    assert_eq!(browser.ui.find_status.current, 1);

    // The keyboard goes back to the document.
    browser.ui.blur();
    browser.activate_surface(SURFACE_PAGE);
    assert!(!browser.ui.find_focused());

    key(&mut browser, Key::Character('g'), ACCELERATOR);
    assert_eq!(browser.ui.find_status.current, 2);
    key(
        &mut browser,
        Key::Character('g'),
        Modifiers {
            shift: true,
            ..ACCELERATOR
        },
    );
    assert_eq!(browser.ui.find_status.current, 1);
    assert!(
        browser.ui.finding(),
        "stepping never took the bar away or gave it the keyboard"
    );
    assert!(!browser.ui.find_focused());
}

/// A search belongs to the page it was made in: another tab has its own, and
/// going somewhere else leaves it behind.
#[test]
fn a_search_belongs_to_its_tab_and_goes_when_the_page_does() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");
    assert_eq!(browser.ui.find_status.total, 3);

    // A second tab is not searching anything, so it shows no bar.
    browser.new_tab();
    go(&mut browser, "example.com");
    assert!(!browser.ui.finding(), "the bar came along to another tab");

    // Back to the first, which still is: the query is the page's, so the
    // bar is the page's search made visible rather than a copy of it.
    browser.select_tab(0);
    assert!(browser.ui.finding());
    assert_eq!(browser.ui.find.text(), "needle");
    assert_eq!(browser.ui.find_status.total, 3);

    // And going somewhere else in that tab leaves the search behind, because
    // the page it was a search of is gone.
    go(&mut browser, "example.com");
    assert!(!browser.ui.finding());
    assert_eq!(browser.ui.find_status.total, 0);
}

/// ⌘C in the bar's field copies what is selected in it, rather than the
/// page's selection or nothing at all.
#[test]
fn command_c_in_the_find_bar_copies_the_bars_own_text() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");
    assert_eq!(browser.ui.find.text(), "needle");

    // Select the whole query the way ⌘A does, then copy it.
    key(&mut browser, Key::Character('a'), ACCELERATOR);
    assert_eq!(browser.ui.find.selected_text(), Some("needle"));
    key(&mut browser, Key::Character('c'), ACCELERATOR);
    assert_eq!(
        browser.clipboard.read().as_deref(),
        Some("needle"),
        "⌘C in the find bar copied something else"
    );
}

/// Ctrl+C on a platform whose accelerator is ⌘ is not a copy — and must not
/// become a character in the query either.
#[test]
fn a_control_key_that_is_not_the_accelerator_types_nothing_into_the_bar() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");

    let control = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    key(&mut browser, Key::Character('c'), control);
    assert_eq!(
        browser.ui.find.text(),
        "needle",
        "a control chord left a character in the query"
    );
    assert_eq!(
        browser.ui.find_status.total, 3,
        "and changed what was found"
    );
}

/// A double-click in the bar's field takes the word under it, the way it
/// does in the address field: selecting is what a copy needs first.
#[test]
fn the_pointer_selects_inside_the_find_bars_field() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");
    frame(&mut browser);

    // Where the field was drawn, from the frame that drew it.
    let field = browser
        .ui
        .describe()
        .into_iter()
        .rfind(|node| node.role == crate::widget::Role::TextInput)
        .expect("the bar's field");
    let (x, y) = (
        field.rect.x + field.rect.width / 2.0,
        field.rect.y + field.rect.height / 2.0,
    );

    browser.on_event(PlatformEvent::PointerMoved { x, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 2 });
    browser.on_event(PlatformEvent::PointerReleased);
    assert_eq!(
        browser.ui.find.selected_text(),
        Some("needle"),
        "a double-click in the field selected nothing"
    );

    key(&mut browser, Key::Character('c'), ACCELERATOR);
    assert_eq!(browser.clipboard.read().as_deref(), Some("needle"));
}

/// What was found is what is selected, so ⌘C copies it — with the bar open
/// and, once the bar has been closed, still.
#[test]
fn the_current_match_is_the_selection_and_can_be_copied() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");

    let page = browser.tabs[0].page.as_ref().expect("a loaded page");
    assert_eq!(
        page.selected_text().as_deref(),
        Some("needle"),
        "the match a reader was taken to is what is selected"
    );

    // With the keyboard in the document, ⌘C copies the page's selection.
    browser.ui.blur();
    browser.activate_surface(SURFACE_PAGE);
    key(&mut browser, Key::Character('c'), ACCELERATOR);
    assert_eq!(browser.clipboard.read().as_deref(), Some("needle"));

    // And closing the bar leaves the last match selected, the way every
    // browser does: the wash goes and what was found stays copyable.
    key(&mut browser, Key::Escape, Modifiers::default());
    let page = browser.tabs[0].page.as_ref().expect("a loaded page");
    assert_eq!(page.match_count(), 0, "the wash is gone");
    assert_eq!(page.selected_text().as_deref(), Some("needle"));
}

/// A resize renumbers every run, and what was selected has to follow the
/// match rather than whatever took its number.
#[test]
fn a_resize_keeps_the_selection_on_the_match_it_was_on() {
    let mut browser = three_needles();
    look_for(&mut browser, "needle");
    frame(&mut browser);

    let mut target = otlyra_gfx::RecordingPainter::default();
    browser.paint(&mut target, Viewport::new(360, 600, 1.0));

    let page = browser.tabs[0].page.as_ref().expect("a loaded page");
    assert_eq!(page.match_count(), 3, "still three of them, laid out anew");
    assert_eq!(
        page.selected_text().as_deref(),
        Some("needle"),
        "the selection followed the match across the relayout"
    );
}
