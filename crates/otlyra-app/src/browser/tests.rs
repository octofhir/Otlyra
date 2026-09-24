//! The browser driven the way a reader drives it, over canned loaders.
//!
//! Navigation, tabs, keys and presses, the inspector, pictures, preferences, zoom
//! and the composited layers, each checked through the events and frames the
//! window would deliver. Its loaders and helpers are shared with the find bar's
//! tests.

use std::sync::Arc;

use otlyra_platform::{FrameRequest, Key, LayerId, Modifiers, Painter, PlatformEvent, Scene};

use super::*;
use crate::fetcher::{Body, Loaded};
use crate::page::PageScene;
use crate::settings;
use crate::ui::{ContextCommand, SystemPage};

use super::frame::{LAYER_CHROME, LAYER_INSPECTOR, LAYER_PAGE};
use super::subresources::IMAGE_CACHE_BUDGET;

/// The smallest PNG that decodes: one opaque pixel.
const ONE_PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// What a fake loader was asked for, shared because the loader itself lives on
/// the fetch thread and a test cannot reach into it.
type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// A loader that serves canned pages, so navigation can be tested without a
/// socket — including the failure path, which a real server makes awkward.
#[derive(Default)]
struct FakeLoader {
    requested: Requests,
}

impl Loader for FakeLoader {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        self.requested
            .lock()
            .expect("no panic on the fetch thread")
            .push(url.to_owned());
        match url {
            "broken.example" => Err("could not fetch broken.example".to_owned()),
            // A `file:` URL loads as itself; anything else becomes an https
            // address, the way a bare hostname does.
            _ if url.starts_with("file://") => Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: format!("<title>Local</title><body><p>Body of {url}").into_bytes(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            }),
            // A bare hostname becomes an https address, the way the real
            // loader normalizes one; an address that already is one is left
            // alone, or going back to it would grow a second scheme each time.
            _ => {
                let final_url = if url.contains("://") {
                    url.to_owned()
                } else {
                    format!("https://{url}/")
                };
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: format!("<title>Title of {url}</title><body><p>Body of {url}")
                        .into_bytes(),
                    charset: Some("utf-8".to_owned()),
                    final_url,
                    ..Default::default()
                })
            }
        }
    }
}

pub(super) fn browser() -> Browser {
    browser_with_log().0
}

/// A browser and the list of what its loader was asked for.
fn browser_with_log() -> (Browser, Requests) {
    let requested = Requests::default();
    let loader = FakeLoader {
        requested: std::sync::Arc::clone(&requested),
    };
    (Browser::new(loader), requested)
}

#[test]
fn a_repeated_pointer_position_requests_no_frame() {
    let mut browser = browser();
    let event = PlatformEvent::PointerMoved { x: 320.0, y: 240.0 };

    assert_eq!(browser.handle_event(event), FrameRequest::Now);
    assert_eq!(
        browser.handle_event(event),
        FrameRequest::None,
        "identical input changes neither hover nor drag geometry"
    );
}

#[test]
fn pointer_motion_across_an_unchanged_page_requests_no_frame() {
    let mut browser = browser();
    assert_eq!(
        browser.handle_event(PlatformEvent::PointerMoved { x: 320.0, y: 240.0 }),
        FrameRequest::Now,
        "leaving the initial off-window position clears any chrome hover"
    );
    assert_eq!(
        browser.handle_event(PlatformEvent::PointerMoved { x: 420.0, y: 340.0 }),
        FrameRequest::None,
        "the cursor moved, but no pixels changed"
    );
}

/// A loading tab turns its mark in the strip whether or not it is the tab
/// being read, so a loading tab anywhere keeps the frames coming.
///
/// This used to be the active tab only, and the strip drew a background
/// tab's spinner once and then left it standing still — which reads as a
/// tab that has stopped rather than one that is working. A frame while a
/// load is in flight costs a chrome rebuild and no page work, which is
/// already what the active tab's own spinner costs.
#[test]
fn any_loading_tab_drives_vsync() {
    let mut browser = browser();
    browser.navigate("example.com");
    assert_eq!(browser.next_frame(), FrameRequest::Vsync);

    browser.new_tab();
    assert_eq!(
        browser.next_frame(),
        FrameRequest::Vsync,
        "the first tab is still loading, and its mark in the strip is still turning"
    );
    assert!(
        browser.spinner_phase().is_some(),
        "so the strip is given a phase to turn it by"
    );
}

/// And with nothing loading anywhere, nothing asks for frames.
#[test]
fn no_loading_tab_drives_no_frames() {
    let mut browser = browser();
    browser.navigate("example.com");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    assert_eq!(browser.next_frame(), FrameRequest::None);
    assert!(browser.spinner_phase().is_none());
}

#[test]
fn a_static_browser_page_has_no_follow_up_frame() {
    let mut browser = browser();
    browser.open_system(SystemPage::About);
    assert_eq!(browser.next_frame(), FrameRequest::None);
}

#[test]
fn an_unchanged_frame_does_not_rebuild_accessibility() {
    let mut browser = browser();
    let mut target = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut target, Viewport::new(800, 600, 1.0));

    assert!(
        browser.accessibility().is_some(),
        "the first tree is published"
    );
    assert!(
        browser.accessibility().is_none(),
        "and remains valid until semantics, geometry or focus changes"
    );

    let _ = browser.handle_event(PlatformEvent::PointerMoved { x: 300.0, y: 300.0 });
    let request = browser.handle_event(PlatformEvent::PointerMoved { x: 400.0, y: 300.0 });
    assert_eq!(request, FrameRequest::None);
    assert!(
        browser.accessibility().is_none(),
        "paint-free pointer motion changes no accessibility nodes"
    );
}

/// Wait for whatever was asked for to arrive.
///
/// Loading happens on another thread and the window is woken when it finishes;
/// a test has no window, so it waits instead.
fn settle(browser: &mut Browser) {
    browser.wait_for_load(std::time::Duration::from_secs(5));
}

/// Navigate and wait, which is what every test means by "load this".
pub(super) fn go(browser: &mut Browser, url: &str) {
    browser.navigate(url);
    settle(browser);
}

fn asked_for(requests: &Requests) -> Vec<String> {
    requests
        .lock()
        .expect("no panic on the fetch thread")
        .clone()
}

fn type_url(browser: &mut Browser, url: &str) {
    // A frame first: the address field's focus id is its place in the order
    // a frame built, so until one has been drawn there is no field to put a
    // caret in. This is the same rule presses follow.
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    browser.ui.focus_address();
    for character in url.chars() {
        browser.on_event(PlatformEvent::TextInput(character));
    }
    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Enter,
        modifiers: Modifiers::default(),
    });
    settle(browser);
}

#[test]
fn typing_an_address_and_pressing_enter_loads_it() {
    let (mut browser, requested) = browser_with_log();
    type_url(&mut browser, "example.com");

    assert_eq!(asked_for(&requested), ["example.com"]);
    assert_eq!(browser.tabs[0].title, "Title of example.com");
    assert!(browser.tabs[0].page.is_some());
}

/// One navigation, one visit — and the visit is where the load *ended up*.
/// The loader normalizes `example.com` to `https://example.com/` the way a
/// redirect would move it, and only the final address is recorded.
#[test]
fn a_navigation_lands_in_the_history_once_with_its_final_url() {
    let mut browser = browser();
    go(&mut browser, "example.com");
    let urls: Vec<&str> = browser
        .history
        .visits()
        .map(|visit| visit.url.as_str())
        .collect();
    assert_eq!(urls, ["https://example.com/"]);

    // The same address again moved nowhere, so it is not a second visit.
    go(&mut browser, "https://example.com/");
    assert_eq!(browser.history.visits().count(), 1);

    // And going back re-reads a place already recorded.
    go(&mut browser, "https://two.example/");
    browser.go_back();
    settle(&mut browser);
    assert_eq!(
        browser.history.visits().count(),
        2,
        "back re-reads, it does not re-visit"
    );
}

#[test]
fn a_press_on_a_history_row_navigates_there() {
    let mut browser = browser();
    go(&mut browser, "example.com");
    browser.open_system(SystemPage::History);

    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(900, 700, 1.0));

    // Walk down the list area until a press lands on the visit's row. The
    // frame was drawn once and presses are tested against it, which is the
    // same rule every surface test follows.
    let navigated = ((UI_HEIGHT as u32 + 120)..680).step_by(4).any(|y| {
        browser.on_event(PlatformEvent::PointerMoved {
            x: 300.0,
            y: f64::from(y),
        });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        settle(&mut browser);
        browser.system_page().is_none()
    });
    assert!(navigated, "a visit's row navigates when pressed");
    assert_eq!(browser.ui.address.text(), "https://example.com/");
}

/// The address bar shows where the load ended up, not what was typed: a
/// redirect that leaves the old text in place is a lie about what is on screen.
#[test]
fn the_address_bar_shows_the_final_url() {
    let mut browser = browser();
    type_url(&mut browser, "example.com");
    assert_eq!(browser.ui.address.text(), "https://example.com/");
}

/// The whole point of the *System* default: the platform saying "dark now"
/// is enough, with no restart and nothing saved.
#[test]
fn the_interface_follows_the_system_appearance_without_a_restart() {
    use crate::widget::theme::Theme;
    let mut browser = browser();
    assert_eq!(browser.ui.theme, Theme::light());

    browser.on_event(PlatformEvent::AppearanceChanged(
        otlyra_platform::ColorScheme::Dark,
    ));
    assert_eq!(browser.ui.theme, Theme::dark());
    assert_eq!(browser.settings.theme, Theme::dark());
    assert_eq!(browser.about.theme, Theme::dark());
}

/// A person who chose a palette chose it over the platform's opinion.
#[test]
fn a_chosen_appearance_outranks_the_system() {
    use crate::widget::theme::Theme;
    let mut browser = browser();
    browser
        .settings
        .settings
        .apply(settings::Action::SetAppearance(settings::Appearance::Light));
    browser.apply_theme();

    browser.on_event(PlatformEvent::AppearanceChanged(
        otlyra_platform::ColorScheme::Dark,
    ));
    assert_eq!(
        browser.ui.theme,
        Theme::light(),
        "Light means light, whatever the platform says"
    );
}

#[test]
fn a_failed_load_keeps_the_tab_and_says_what_happened() {
    let mut browser = browser();
    type_url(&mut browser, "broken.example");

    assert_eq!(browser.tabs.len(), 1);
    assert!(browser.tabs[0].page.is_none());
    assert!(
        browser.tabs[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("broken.example"))
    );
}

#[test]
fn tabs_are_opened_selected_and_closed() {
    let mut browser = browser();
    type_url(&mut browser, "first.example");

    browser.new_tab();
    assert_eq!(browser.tabs.len(), 2);
    assert_eq!(browser.active, 1);
    type_url(&mut browser, "second.example");

    browser.select_tab(0);
    assert_eq!(browser.active, 0);
    assert_eq!(
        browser.ui.address.text(),
        "https://first.example/",
        "switching tabs puts that tab's address back"
    );

    browser.close_tab(0);
    assert_eq!(browser.tabs.len(), 1);
    assert_eq!(browser.tabs[0].title, "Title of second.example");
}

#[test]
fn closing_the_last_tab_empties_it_rather_than_leaving_no_tabs() {
    let mut browser = browser();
    type_url(&mut browser, "example.com");
    browser.close_tab(0);

    assert_eq!(browser.tabs.len(), 1);
    assert!(browser.tabs[0].page.is_none());
    assert_eq!(browser.ui.address.text(), "");
}

/// Each tab scrolls independently: a scroll in one is not a scroll in another.
#[test]
fn scrolling_belongs_to_the_active_tab() {
    let mut browser = browser();
    type_url(&mut browser, "long.example");
    browser.new_tab();
    type_url(&mut browser, "other.example");

    // Paint so both pages have a layout to clamp a scroll against.
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.ui.pointer_moved(400.0, 400.0, &mut browser.text);
    browser.on_event(PlatformEvent::Scroll {
        x: 0.0,
        y: 50.0,
        source: otlyra_platform::ScrollSource::Wheel,
        modifiers: Default::default(),
    });

    let active = browser.active;
    assert_eq!(
        browser.tabs[1 - active]
            .page
            .as_ref()
            .expect("page")
            .scroll(),
        0.0
    );
}

#[test]
fn a_scroll_over_the_interface_does_not_scroll_the_page() {
    let mut browser = browser();
    type_url(&mut browser, "example.com");
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.ui.pointer_moved(400.0, 10.0, &mut browser.text);
    browser.on_event(PlatformEvent::Scroll {
        x: 0.0,
        y: 100.0,
        source: otlyra_platform::ScrollSource::Wheel,
        modifiers: Default::default(),
    });
    assert_eq!(browser.tabs[0].page.as_ref().expect("page").scroll(), 0.0);
}

/// Clicking a link navigates, and the address it navigates to is resolved
/// against the page the link was on — a relative href is meaningless otherwise.
#[test]
fn clicking_a_link_navigates_to_it() {
    let mut browser = Browser::new(LinkLoader);
    browser.navigate("start.example");
    settle(&mut browser);

    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    let (x, y) = link_position(&browser);
    browser.on_event(PlatformEvent::PointerMoved { x, y });
    assert_eq!(
        browser.cursor(),
        Cursor::Pointer,
        "the pointer says so first"
    );

    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    assert_eq!(browser.tabs[0].url, "https://start.example/next");
}

/// Dragging a tab along the strip reorders the browser's own tabs, and the
/// tab being read stays the tab being read wherever it lands.
#[test]
fn dragging_a_tab_reorders_the_strip() {
    let mut browser = Browser::new(LinkLoader);
    browser.new_tab();
    browser.new_tab();
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(1000, 700, 1.0),
    );
    let order: Vec<TabId> = browser.tabs.iter().map(|tab| tab.id).collect();
    assert_eq!(browser.tabs[browser.active].id, order[2]);

    let places = browser.ui().tab_places();
    let rect_of = |id: TabId| {
        places
            .borrow()
            .iter()
            .find(|(key, _)| *key == id.0)
            .map(|(_, rect)| *rect)
            .expect("every tab was drawn")
    };
    let last = rect_of(order[2]);
    let first = rect_of(order[0]);

    // Press the last tab and carry it to the front.
    browser.on_event(PlatformEvent::PointerMoved {
        x: last.x + last.width / 2.0,
        y: last.y + last.height / 2.0,
    });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    browser.on_event(PlatformEvent::PointerMoved {
        x: first.x + first.width / 2.0,
        y: last.y + last.height / 2.0,
    });
    browser.on_event(PlatformEvent::PointerReleased);

    assert_eq!(
        browser.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        vec![order[2], order[0], order[1]],
        "the dragged tab did not land where it was dropped"
    );
    assert_eq!(
        browser.tabs[browser.active].id, order[2],
        "the tab being read must still be the tab being read"
    );
}

/// Put the caret in the address bar the way a reader does, so the browser
/// knows the chrome is what the keyboard belongs to.
fn name_the_address_bar(browser: &mut Browser) {
    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Character('l'),
        modifiers: Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        },
    });
    assert!(browser.ui().address_focused(), "the caret is in the field");
}

/// Tab walks the document: links and fields, in the order they are written,
/// with the ring following. Without it a reader without a pointer cannot
/// reach anything on a page at all.
#[test]
fn tab_walks_the_page_and_leaves_it_at_the_end() {
    struct TwoLinks;

    impl Loader for TwoLinks {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<title>Two</title><body>\
                        <p><a href=\"/first\">first</a> and <a href=\"/second\">second</a></p>\
                        <input id=field value=\"typed\">"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(TwoLinks);
    go(&mut browser, "https://walk.example/");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(900, 700, 1.0),
    );
    // A press on the page is what makes the document the active surface,
    // the same way a reader starts reading before they start walking.
    browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    browser.on_event(PlatformEvent::PointerReleased);

    let tab = |browser: &mut Browser, shift: bool| {
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Tab,
            modifiers: Modifiers {
                shift,
                ..Modifiers::default()
            },
        });
    };
    let focused_link = |browser: &Browser| {
        browser.tabs[browser.active]
            .page
            .as_ref()
            .and_then(PageScene::focused_link)
    };

    tab(&mut browser, false);
    assert_eq!(
        focused_link(&browser).as_deref(),
        Some("/first"),
        "Tab reached nothing, so the page cannot be walked at all"
    );
    tab(&mut browser, false);
    assert_eq!(focused_link(&browser).as_deref(), Some("/second"));
    tab(&mut browser, true);
    assert_eq!(
        focused_link(&browser).as_deref(),
        Some("/first"),
        "shift-Tab walks back the way it came"
    );

    // Return on a link the keyboard reached follows it, resolved against
    // the page it was on.
    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Enter,
        modifiers: Modifiers::default(),
    });
    settle(&mut browser);
    assert_eq!(
        browser.tabs[browser.active].url,
        "https://walk.example/first"
    );
}

/// And back again: at the end of the toolbar's own order the keyboard
/// returns to the document, entering from the end the reader is coming in
/// at. Without this the chrome is its own trap and the page is unreachable
/// once the keyboard has left it.
#[test]
fn tab_off_the_end_of_the_chrome_goes_back_into_the_page() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(900, 700, 1.0),
    );

    // Walk the chrome until the keyboard runs off its end, which is where
    // the document begins again.
    let mut entered = false;
    for _ in 0..40 {
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Tab,
            modifiers: Modifiers::default(),
        });
        if browser.keyboard_surface == SURFACE_PAGE {
            entered = true;
            break;
        }
    }
    assert!(entered, "the toolbar kept the keyboard to itself");
    assert_eq!(
        browser.tabs[browser.active]
            .page
            .as_ref()
            .and_then(PageScene::focused_link)
            .as_deref(),
        Some("/next"),
        "coming in forwards lands on the first thing in the document"
    );
}

/// `tabindex` orders a page's own traversal: a positive value comes before
/// everything the browser would have walked by default, and a negative one
/// is reachable but never walked to.
#[test]
fn tabindex_orders_the_walk_and_takes_things_out_of_it() {
    struct Indexed;

    impl Loader for Indexed {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<title>Order</title><body>\
                        <a href=\"/plain\">plain</a>\
                        <a href=\"/skipped\" tabindex=\"-1\">skipped</a>\
                        <a href=\"/first\" tabindex=\"1\">first</a>"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(Indexed);
    go(&mut browser, "https://order.example/");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(900, 700, 1.0),
    );
    browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    browser.on_event(PlatformEvent::PointerReleased);

    let mut reached = Vec::new();
    for _ in 0..2 {
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Tab,
            modifiers: Modifiers::default(),
        });
        reached.push(
            browser.tabs[browser.active]
                .page
                .as_ref()
                .and_then(PageScene::focused_link),
        );
    }

    assert_eq!(
        reached,
        vec![Some("/first".to_owned()), Some("/plain".to_owned())],
        "a positive tabindex comes first and a negative one is not walked to"
    );
}

/// Past the last thing on the page, the keyboard goes to the browser around
/// it: a document that trapped Tab would be a document a reader could not
/// leave without a pointer.
#[test]
fn tab_past_the_end_of_the_page_reaches_the_interface() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(900, 700, 1.0),
    );
    browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    browser.on_event(PlatformEvent::PointerReleased);

    // The page the loader serves has one link, so two steps run off it.
    for _ in 0..2 {
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Tab,
            modifiers: Modifiers::default(),
        });
    }

    assert!(
        browser.ui().focused().is_some(),
        "the keyboard left the page and landed nowhere"
    );
    assert!(
        browser.tabs[browser.active]
            .page
            .as_ref()
            .and_then(PageScene::focused_link)
            .is_none(),
        "the page kept the focus it handed on"
    );
}

/// Typing in the omnibox offers where the reader has been and what they
/// kept, and Return takes what the arrows reached rather than what was
/// typed — which is the whole reason to offer anything.
#[test]
fn typing_offers_places_and_the_arrows_take_one() {
    let (mut browser, requests) = browser_with_log();
    go(&mut browser, "start.example");
    browser
        .bookmarks
        .add("https://kept.example/page", "A kept page");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(1000, 700, 1.0),
    );

    name_the_address_bar(&mut browser);
    browser.ui.address.clear();
    for character in "example".chars() {
        browser.on_event(PlatformEvent::TextInput(character));
    }
    assert!(
        browser.ui().suggesting(),
        "typing something both stores know offered nothing"
    );

    // A kept page comes before a place merely visited: it was kept because
    // the reader meant to come back to it.
    let offered: Vec<String> = browser
        .ui()
        .suggestions()
        .iter()
        .map(|row| row.url.clone())
        .collect();
    assert_eq!(
        offered.first().map(String::as_str),
        Some("https://kept.example/page")
    );
    assert!(offered.iter().any(|url| url.contains("start.example")));
    assert!(browser.ui().suggestions()[0].kept);

    // Down marks the first row without touching what was typed, and Return
    // takes the marked one.
    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Down,
        modifiers: Modifiers::default(),
    });
    assert_eq!(
        browser.ui().address.text(),
        "example",
        "walking the list rewrote the field"
    );
    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Enter,
        modifiers: Modifiers::default(),
    });
    settle(&mut browser);
    assert_eq!(
        asked_for(&requests).last().map(String::as_str),
        Some("https://kept.example/page"),
        "Return took what was typed rather than what the arrows reached"
    );
    assert!(!browser.ui().suggesting(), "the list outlived the choice");
}

/// Escape puts the list away and leaves the caret where it was.
#[test]
fn escape_puts_the_offered_places_away_and_keeps_the_caret() {
    let (mut browser, _requests) = browser_with_log();
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(1000, 700, 1.0),
    );
    name_the_address_bar(&mut browser);
    browser.ui.address.clear();
    for character in "start".chars() {
        browser.on_event(PlatformEvent::TextInput(character));
    }
    assert!(browser.ui().suggesting());

    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Escape,
        modifiers: Modifiers::default(),
    });
    assert!(!browser.ui().suggesting(), "Escape left the list showing");
    assert!(
        browser.ui().address_focused(),
        "Escape took the caret out of the field as well as the list"
    );
}

/// A press on the page while the list is showing is a press on the page:
/// the list has no sheet, so it dismisses on the way through rather than
/// swallowing the click the reader meant.
#[test]
fn a_press_on_the_page_goes_through_the_suggestions() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(1000, 700, 1.0),
    );
    name_the_address_bar(&mut browser);
    browser.ui.address.clear();
    for character in "start".chars() {
        browser.on_event(PlatformEvent::TextInput(character));
    }
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(1000, 700, 1.0),
    );
    assert!(browser.ui().suggesting());

    let (x, y) = link_position(&browser);
    browser.on_event(PlatformEvent::PointerMoved { x, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    settle(&mut browser);

    assert!(!browser.ui().suggesting(), "the list stayed over the page");
    assert_eq!(
        browser.tabs[browser.active].url, "https://start.example/next",
        "the press that dismissed the list never reached the link"
    );
}

/// Ask for a menu where the pointer is, the way the platform does.
fn ask_for_a_menu(browser: &mut Browser, x: f64, y: f64) {
    browser.on_event(PlatformEvent::PointerMoved { x, y });
    browser.on_event(PlatformEvent::ContextMenuRequested);
}

/// What the open context menu offers, in the order it offers it.
fn context_rows(browser: &Browser) -> Vec<ContextCommand> {
    browser
        .ui()
        .describe()
        .into_iter()
        .filter(|node| node.role == crate::widget::Role::MenuItem)
        .filter_map(|node| {
            [
                ContextCommand::OpenLinkInNewTab,
                ContextCommand::CopyLinkAddress,
                ContextCommand::CopySelection,
                ContextCommand::SelectAll,
                ContextCommand::Back,
                ContextCommand::Forward,
                ContextCommand::Reload,
                ContextCommand::InspectElement,
            ]
            .into_iter()
            .find(|command| command.label() == node.label)
        })
        .collect()
}

/// A menu asked for over a link offers what can be done with a link, and
/// what it offers is decided from the document rather than guessed.
#[test]
fn a_menu_asked_for_over_a_link_offers_the_link() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );

    let (x, y) = link_position(&browser);
    ask_for_a_menu(&mut browser, x, y);
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );

    let rows = context_rows(&browser);
    assert_eq!(
        rows.first(),
        Some(&ContextCommand::OpenLinkInNewTab),
        "the link is what the reader asked about, so it comes first"
    );
    assert!(rows.contains(&ContextCommand::CopyLinkAddress));
    assert!(rows.contains(&ContextCommand::InspectElement));
    assert!(
        !rows.contains(&ContextCommand::CopySelection),
        "nothing is selected, so there is nothing to copy"
    );

    // And choosing the first row opens the link the press landed on — in a
    // tab of its own, with the one being read left where it was.
    let before = browser.tabs.len();
    let panel = browser
        .ui()
        .describe()
        .into_iter()
        .position(|node| node.label == ContextCommand::OpenLinkInNewTab.label())
        .expect("the row that was just described");
    let action = browser.ui.activate_described(panel, &mut browser.text);
    browser.apply(action);
    settle(&mut browser);

    assert_eq!(browser.tabs.len(), before + 1);
    assert_eq!(browser.tabs[before].url, "https://start.example/next");
    assert_eq!(browser.tabs[0].url, "https://start.example/");
    assert!(!browser.ui().popup_open(), "choosing a row closes the menu");
}

/// Away from a link the same menu is the page's own, and Back says whether
/// there is anywhere to go back to rather than pretending there is.
#[test]
fn a_menu_over_the_page_offers_what_the_page_can_do() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );

    ask_for_a_menu(&mut browser, 700.0, 500.0);
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );

    let rows = context_rows(&browser);
    assert_eq!(
        rows,
        vec![
            ContextCommand::Back,
            ContextCommand::Forward,
            ContextCommand::Reload,
            ContextCommand::SelectAll,
            ContextCommand::InspectElement,
        ]
    );
    let back = browser
        .ui()
        .describe()
        .into_iter()
        .find(|node| node.label == ContextCommand::Back.label())
        .expect("the back row");
    assert!(
        !back.enabled,
        "there is nowhere to go back to, and a row that says otherwise is a lie"
    );
}

/// A press for a menu never reaches the page behind it: it does not follow
/// the link it lands on, and it does not start a selection.
#[test]
fn asking_for_a_menu_over_a_link_does_not_follow_it() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );

    let (x, y) = link_position(&browser);
    ask_for_a_menu(&mut browser, x, y);
    settle(&mut browser);

    assert_eq!(browser.tabs[0].url, "https://start.example/");
    assert!(browser.ui().popup_open());
}

/// The document the menu was asked about is gone, and so are its rows.
#[test]
fn navigating_puts_away_a_menu_asked_for_on_the_page() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );
    ask_for_a_menu(&mut browser, 700.0, 500.0);
    assert!(browser.ui().popup_open());

    browser.navigate("start.example/elsewhere");
    assert!(
        !browser.ui().popup_open(),
        "the menu outlived the page it described"
    );
    settle(&mut browser);
}

/// So is a menu whose window has just been resized under it.
#[test]
fn resizing_puts_away_a_menu_asked_for_on_the_page() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );
    ask_for_a_menu(&mut browser, 700.0, 500.0);
    assert!(browser.ui().popup_open());

    browser.on_event(PlatformEvent::Resized(Viewport::new(640, 480, 1.0)));
    assert!(!browser.ui().popup_open());
}

/// A menu asked for over the interface is not the page's menu: the browser
/// has its own, and offering "Inspect Element" over the toolbar would be
/// offering to inspect something the press did not land on.
#[test]
fn asking_for_a_menu_over_the_toolbar_offers_nothing() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );

    ask_for_a_menu(&mut browser, 400.0, UI_HEIGHT - 20.0);
    assert!(!browser.ui().popup_open());
}

#[test]
fn the_cursor_is_ordinary_away_from_a_link() {
    let mut browser = Browser::new(LinkLoader);
    browser.navigate("start.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
    assert_eq!(browser.cursor(), Cursor::Default);

    // Over the empty end of the tab strip, where nothing responds.
    browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 10.0 });
    assert_eq!(browser.cursor(), Cursor::Default);

    // Over a tab, which does: the hand is a promise that pressing does
    // something, and it is owed by the interface as much as by a link.
    browser.on_event(PlatformEvent::PointerMoved { x: 100.0, y: 10.0 });
    assert_eq!(browser.cursor(), Cursor::Pointer);

    // And over the address field, where text goes.
    browser.on_event(PlatformEvent::PointerMoved {
        x: 400.0,
        y: UI_HEIGHT - 20.0,
    });
    assert_eq!(browser.cursor(), Cursor::Text);
}

#[test]
fn a_press_on_the_page_that_is_not_a_link_navigates_nowhere() {
    let mut browser = Browser::new(LinkLoader);
    browser.navigate("start.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    assert_eq!(browser.tabs[0].url, "https://start.example/");
}

/// A drag across the page selects the words the pointer passed, and ⌘C puts
/// them on the clipboard.
#[test]
fn dragging_across_the_page_selects_text_and_copies_it() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    // Across the first line of the page, which is the paragraph the loader
    // serves. The pointer is moved first, because a press lands where the
    // pointer last was.
    let line = browser.tabs[0]
        .page
        .as_ref()
        .expect("a page")
        .rect_of(
            browser.tabs[0]
                .page
                .as_ref()
                .expect("a page")
                .box_at(30.0, UI_HEIGHT + 20.0)
                .expect("something under the pointer"),
        )
        .expect("it was drawn");

    let y = f64::from(line.y) + UI_HEIGHT + 6.0;
    browser.on_event(PlatformEvent::PointerMoved { x: 9.0, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    browser.on_event(PlatformEvent::PointerMoved { x: 400.0, y });
    browser.on_event(PlatformEvent::PointerReleased);

    let selected = browser.tabs[0]
        .page
        .as_ref()
        .expect("a page")
        .selected_text()
        .expect("a drag across the words selected some of them");
    assert!(
        selected.contains("go on") || selected.contains("go"),
        "the words the pointer passed: {selected:?}"
    );

    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Character('c'),
        modifiers: Modifiers {
            command: true,
            ..Modifiers::default()
        },
    });
    assert_eq!(
        browser.clipboard.read().as_deref(),
        Some(selected.as_str()),
        "what was selected is what was copied"
    );
}

/// An open menu is drawn over the page and owns every press that lands on
/// it — including the second of a double click, which would otherwise
/// select a word behind the menu instead of choosing the item under the
/// pointer.
#[test]
fn a_press_on_an_open_menu_never_reaches_the_page_behind_it() {
    let mut browser = Browser::new(LinkLoader);
    go(&mut browser, "start.example");
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.ui.open_menu();
    browser.on_event(PlatformEvent::PointerMoved {
        x: 700.0,
        y: UI_HEIGHT + 40.0,
    });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });

    assert!(
        !browser.selecting,
        "the page took a press that belonged to the menu"
    );
    assert!(
        !browser.tabs[0]
            .page
            .as_ref()
            .expect("a page")
            .has_selection(),
        "and started selecting behind it"
    );
    assert!(
        !browser.ui.menu_open(),
        "the interface got the press, and a press outside an open menu \
             closes it"
    );
}

/// The second rank of selecting: a word, the block it is in, the whole page,
/// and the far end moved by the keyboard.
#[test]
fn a_second_click_takes_a_word_and_a_third_takes_the_block() {
    /// Two paragraphs of ordinary words, so a word and a block are
    /// different amounts of text.
    struct Prose;
    impl Loader for Prose {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<body><p>alpha beta gamma</p><p>delta epsilon</p>".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(Prose);
    go(&mut browser, "prose.example");
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    let selected = |browser: &Browser| {
        browser.tabs[0]
            .page
            .as_ref()
            .expect("a page")
            .selected_text()
            .unwrap_or_default()
    };

    // Into the first word of the first paragraph.
    let y = UI_HEIGHT + 14.0;
    browser.on_event(PlatformEvent::PointerMoved { x: 12.0, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 2 });
    browser.on_event(PlatformEvent::PointerReleased);
    assert_eq!(selected(&browser), "alpha", "a second click takes the word");

    browser.on_event(PlatformEvent::PointerPressed { clicks: 3 });
    browser.on_event(PlatformEvent::PointerReleased);
    assert_eq!(
        selected(&browser),
        "alpha beta gamma",
        "a third takes the block it is in and stops there"
    );

    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Character('a'),
        modifiers: Modifiers {
            command: true,
            ..Modifiers::default()
        },
    });
    let everything = selected(&browser);
    assert!(
        everything.contains("alpha beta gamma") && everything.contains("delta epsilon"),
        "and ⌘A takes the page: {everything:?}"
    );

    // Back to one word, then one character further with the keyboard.
    browser.on_event(PlatformEvent::PointerPressed { clicks: 2 });
    browser.on_event(PlatformEvent::PointerReleased);
    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Right,
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    });
    assert_eq!(
        selected(&browser),
        "alpha ",
        "shift and an arrow move the far end and keep the near one"
    );

    // An arrow with nothing held down is still the page scrolling, which is
    // what it means on a page nobody is editing.
    let before = selected(&browser);
    browser.on_event(PlatformEvent::KeyPressed {
        key: Key::Right,
        modifiers: Modifiers::default(),
    });
    assert_eq!(selected(&browser), before, "and a bare arrow moves nothing");
}

/// A loader whose page brings a font with it, from a stylesheet in a
/// directory of its own — so the address is only right if it is resolved
/// against the sheet rather than against the page.
struct FontLoader;

impl Loader for FontLoader {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        let page = |bytes: Vec<u8>, kind: &str| {
            Ok(Loaded {
                content_type: Some(kind.to_owned()),
                bytes,
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        };
        match url {
            "https://type.example/" => page(
                b"<link rel=stylesheet href=/style/page.css><p>set in it".to_vec(),
                "text/html",
            ),
            "https://type.example/style/page.css" => page(
                b"@font-face { font-family: Brought; src: url(../fonts/brought.ttf) }\n\
                      p { font-family: Brought }"
                    .to_vec(),
                "text/css",
            ),
            "https://type.example/fonts/brought.ttf" => {
                page(otlyra_text::TEST_FONT.to_vec(), "font/ttf")
            }
            other => Err(format!("404 {other}")),
        }
    }
}

/// A page that brings its own typeface gets it: the rule is found in the
/// fetched sheet, the address is resolved against that sheet, and the family
/// is one the shaper can answer for afterwards.
#[test]
fn a_page_brings_its_own_font() {
    let mut browser = Browser::new(FontLoader);
    assert!(
        !browser.text.has_family("Brought"),
        "the family cannot exist before the page that defines it"
    );

    go(&mut browser, "https://type.example/");
    // A frame: a `@font-face` rule is only known once the sheet holding it has
    // been parsed, which is the page's first restyle.
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    settle(&mut browser);
    browser.prepare_frame(
        Viewport::new(800, 600, 1.0),
        std::time::Duration::from_secs(5),
    );

    assert!(
        browser.text.has_family("Brought"),
        "the family the page defined is the shaper's now"
    );
    assert!(
        browser
            .fetcher
            .exchanges()
            .iter()
            .any(|exchange| exchange.url == "https://type.example/fonts/brought.ttf"),
        "the address is resolved against the sheet, not the page: {:?}",
        browser
            .fetcher
            .exchanges()
            .iter()
            .map(|exchange| exchange.url.clone())
            .collect::<Vec<_>>()
    );
}

/// A loader that serves a fixed set of addresses, logs every request, and
/// answers anything else with a 404.
struct TableLoader {
    requested: Requests,
    /// Address, content type and body.
    table: &'static [(&'static str, &'static str, &'static [u8])],
    /// The address that is slow to answer, if any, and by how much.
    slow: Option<(&'static str, std::time::Duration)>,
}

impl Loader for TableLoader {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        self.requested
            .lock()
            .expect("no panic on the fetch thread")
            .push(url.to_owned());
        if let Some((slow, delay)) = self.slow
            && slow == url
        {
            std::thread::sleep(delay);
        }
        let (_, kind, bytes) = self
            .table
            .iter()
            .find(|(address, _, _)| *address == url)
            .ok_or_else(|| format!("404 {url}"))?;
        Ok(Loaded {
            content_type: Some((*kind).to_owned()),
            bytes: bytes.to_vec(),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

/// A browser over `table`, and the log of what it asked for.
fn table_browser(
    table: &'static [(&'static str, &'static str, &'static [u8])],
    slow: Option<(&'static str, std::time::Duration)>,
) -> (Browser, Requests) {
    let requested = Requests::default();
    let browser = Browser::new(TableLoader {
        requested: std::sync::Arc::clone(&requested),
        table,
        slow,
    });
    (browser, requested)
}

/// Draw a frame and let what it asks for arrive: the rules that name a
/// background or a font are only computed on the way to one.
fn frame_and_settle(browser: &mut Browser) {
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    settle(browser);
    browser.prepare_frame(
        Viewport::new(800, 600, 1.0),
        std::time::Duration::from_secs(5),
    );
}

/// A background a linked sheet names is fetched from beside the sheet, not
/// from beside the page (CSS Values 4 §4.5.1).
#[test]
fn a_background_is_fetched_beside_its_sheet() {
    let (mut browser, requested) = table_browser(
        &[
            (
                "https://bg.example/page/index.html",
                "text/html",
                b"<link rel=stylesheet href=/assets/css/site.css><div class=logo></div>",
            ),
            (
                "https://bg.example/assets/css/site.css",
                "text/css",
                b".logo { width: 10px; height: 10px; background: url(../img/logo.png) }",
            ),
            (
                "https://bg.example/assets/img/logo.png",
                "image/png",
                ONE_PIXEL_PNG,
            ),
        ],
        None,
    );
    go(&mut browser, "https://bg.example/page/index.html");
    frame_and_settle(&mut browser);

    let asked = asked_for(&requested);
    assert!(
        asked
            .iter()
            .any(|url| url == "https://bg.example/assets/img/logo.png"),
        "{asked:?}"
    );
    assert!(
        !asked
            .iter()
            .any(|url| url.contains("/page/") && url.ends_with("logo.png")),
        "resolved against the page: {asked:?}"
    );
}

/// A sheet's `url()`s come out absolute, and a page from the network is still
/// asked whether it may reach them: neither a background nor a font on disk
/// is fetched for it.
#[test]
fn a_sheet_may_not_name_the_disk() {
    let (mut browser, requested) = table_browser(
        &[(
            "https://disk.example/",
            "text/html",
            b"<style>\
                @font-face { font-family: Disk; src: url(file:///etc/disk.woff2) }\
                div { width: 10px; height: 10px; font-family: Disk;\
                      background: url(file:///etc/disk.png) }\
              </style><div>x</div>",
        )],
        None,
    );
    go(&mut browser, "https://disk.example/");
    frame_and_settle(&mut browser);

    let asked = asked_for(&requested);
    assert_eq!(asked, ["https://disk.example/"]);
}

/// A base element moves where the markup's addresses point — a picture and a
/// link both — and moves nothing about what the page may reach: that is still
/// the document's own address's to decide.
#[test]
fn a_base_element_moves_links_and_pictures() {
    let (mut browser, requested) = table_browser(
        &[
            (
                "https://doc.example/dir/page.html",
                "text/html",
                b"<base href=\"https://cdn.example/root/\">\
                  <body><p><a href=next.html>go on</a></p><img src=pic.png>",
            ),
            (
                "https://cdn.example/root/pic.png",
                "image/png",
                ONE_PIXEL_PNG,
            ),
            (
                "https://doc.example/secret.html",
                "text/html",
                b"<base href=\"file:///tmp/\"><body><img src=secret.png>",
            ),
        ],
        None,
    );
    go(&mut browser, "https://doc.example/dir/page.html");
    assert!(
        asked_for(&requested)
            .iter()
            .any(|url| url == "https://cdn.example/root/pic.png"),
        "{:?}",
        asked_for(&requested)
    );

    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    let (x, y) = link_position(&browser);
    browser.on_event(PlatformEvent::PointerMoved { x, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    assert_eq!(browser.tabs[0].url, "https://cdn.example/root/next.html");

    // A page from the network with a base on disk: its picture resolves to a
    // file, and the page may not read one.
    go(&mut browser, "https://doc.example/secret.html");
    let asked = asked_for(&requested);
    assert!(
        !asked.iter().any(|url| url.starts_with("file:")),
        "{asked:?}"
    );
}

/// An `@import` is fetched from beside the sheet that names it, and the page
/// is not drawn until it has arrived — it is as much the page's style as the
/// sheet that imports it.
#[test]
fn an_import_is_fetched_relative_to_its_sheet_and_holds_the_first_frame() {
    let (mut browser, requested) = table_browser(
        &[
            (
                "https://imp.example/",
                "text/html",
                b"<title>T</title><link rel=stylesheet href=/css/main.css><body><p>text",
            ),
            (
                "https://imp.example/css/main.css",
                "text/css",
                b"@import \"parts/colour.css\";",
            ),
            (
                "https://imp.example/css/parts/colour.css",
                "text/css",
                b"p { color: #008000 }",
            ),
        ],
        // Long enough to outlast the wait below by a wide margin, short enough
        // that the test does not hang.
        Some((
            "https://imp.example/css/parts/colour.css",
            std::time::Duration::from_millis(1500),
        )),
    );
    browser.navigate("https://imp.example/");
    // Until the import is asked for, the link that names it is still
    // outstanding and would hold the frame by itself; from then on the import
    // is all there is left to wait for.
    let colour = "https://imp.example/css/parts/colour.css";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !asked_for(&requested).iter().any(|url| url == colour) {
        assert!(
            std::time::Instant::now() < deadline,
            "the import was never asked for"
        );
        for fetched in browser.fetcher.wait(std::time::Duration::from_millis(20)) {
            browser.receive(fetched);
        }
    }
    let active = browser.active;
    assert!(
        browser.blocked_on_style(active),
        "the import is outstanding"
    );

    settle(&mut browser);
    assert!(!browser.blocked_on_style(active), "the import arrived");
    let page = browser.tabs[active].page.as_mut().expect("a page");
    page.build_display_list(&mut TextEngine::isolated(), 800.0, 600.0, 0.0);
    let boxes = page.boxes();
    let coloured = boxes
        .descendants(boxes.root())
        .into_iter()
        .any(|id| boxes.node(id).style.color == otlyra_gfx::peniko::Color::from_rgb8(0, 128, 0));
    assert!(coloured, "the imported rule reached the box tree");
}

/// A loader whose pages contain one link, so the click path has something to
/// land on.
struct LinkLoader;

impl Loader for LinkLoader {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        // Anything that is not already an address on this host is treated as
        // its root, which is what typing a bare hostname means.
        let path = match url.strip_prefix("https://start.example") {
            Some("") | None => "/",
            Some(path) => path,
        };
        Ok(Loaded {
            content_type: Some("text/html".to_owned()),
            bytes: b"<title>Linked</title><body><p><a href=\"/next\">go on</a></p>".to_vec(),
            charset: Some("utf-8".to_owned()),
            final_url: format!("https://start.example{path}"),
            ..Default::default()
        })
    }
}

#[test]
fn the_inspector_takes_its_height_out_of_the_page_rather_than_over_it() {
    let mut browser = Browser::new(LinkLoader);
    browser.navigate("start.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    let content = 600.0 - UI_HEIGHT;
    assert_eq!(
        browser.dock_height(content),
        0.0,
        "closed, it takes nothing"
    );

    browser.inspector.toggle();
    let dock = browser.dock_height(content);
    assert!(dock > 0.0);
    assert!(
        dock < content,
        "the page keeps room to be a page: {dock} of {content}"
    );
    // The panel starts where the page stops, so neither is drawn over the
    // other and the page is laid out for the height it actually has.
    assert_eq!(browser.dock_top(), 600.0 - dock);
}

#[test]
fn the_picker_chooses_the_element_a_click_would_have_hit() {
    let mut browser = Browser::new(LinkLoader);
    browser.navigate("start.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.inspector.toggle();
    browser.inspector.picking = true;

    // Where the link is — the same point that would follow it if the picker
    // were not armed, which is the property worth having: one hit test, two
    // readings of the answer.
    let (x, y) = link_position(&browser);
    browser.on_event(PlatformEvent::PointerMoved { x, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });

    assert_eq!(
        browser.tabs[0].url, "https://start.example/",
        "armed, the press picked rather than followed the link"
    );
    let selected = browser.inspector.selected.expect("something was chosen");
    let page = browser.tabs[0].page.as_ref().expect("a page is loaded");
    let named = page
        .document()
        .get(selected)
        .map(|node| match &node.data {
            otlyra_dom::NodeData::Element(element) => element.name.local.to_string(),
            otlyra_dom::NodeData::Text(_) => "#text".to_owned(),
            _ => "other".to_owned(),
        })
        .expect("the chosen node is in the document it came from");
    assert!(
        ["a", "p", "body", "#text"].contains(&named.as_str()),
        "picked {named}, which is not on the line that was pointed at"
    );

    // And the picker disarms itself, so the next press is a press again.
    assert!(!browser.inspector.picking);
}

#[test]
fn moving_the_picker_between_page_elements_requests_each_highlight_frame() {
    struct TwoBlocks;

    impl Loader for TwoBlocks {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<style>div { height: 80px }</style>\
                             <body><div>one</div><div>two</div>"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(TwoBlocks);
    browser.navigate("https://blocks.example/");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    let positions = {
        let page = browser.tabs[0].page.as_ref().expect("a page is loaded");
        let boxes = page.boxes();
        boxes
            .descendants(boxes.root())
            .into_iter()
            .filter(|&id| {
                boxes
                    .node(id)
                    .tag
                    .as_ref()
                    .is_some_and(|tag| tag.as_ref() == "div")
            })
            .map(|id| {
                let rect = page.rect_of(id).expect("the block was laid out");
                (
                    f64::from(rect.x + rect.width / 2.0),
                    f64::from(rect.y + rect.height / 2.0),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(positions.len(), 2);

    browser.inspector.toggle();
    browser.inspector.picking = true;

    let (first_x, first_y) = positions[0];
    assert_eq!(
        browser.handle_event(PlatformEvent::PointerMoved {
            x: first_x,
            y: first_y,
        }),
        FrameRequest::Now
    );
    let first = browser
        .inspector
        .selected
        .expect("the first block was highlighted");

    let (second_x, second_y) = positions[1];
    assert_eq!(
        browser.handle_event(PlatformEvent::PointerMoved {
            x: second_x,
            y: second_y,
        }),
        FrameRequest::Now,
        "a new picker target changes overlay pixels even inside an otherwise static page"
    );
    assert_ne!(
        browser.inspector.selected,
        Some(first),
        "the second point must exercise a different element"
    );
}

#[test]
fn the_highlight_is_where_the_engine_drew_the_chosen_box() {
    let mut browser = Browser::new(LinkLoader);
    browser.navigate("start.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.inspector.toggle();
    let (x, y) = link_position(&browser);
    browser.inspector.picking = true;
    browser.on_event(PlatformEvent::PointerMoved { x, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });

    let rect = browser
        .chosen_box()
        .expect("the chosen box was drawn")
        .border;
    // Asserted against the engine's own answer rather than against numbers:
    // whatever box the hit test names, the overlay is that box's rectangle.
    let page = browser.tabs[0].page.as_ref().expect("a page is loaded");
    let id = page
        .boxes()
        .box_for(browser.inspector.selected.expect("something was chosen"))
        .expect("the chosen node has a box");
    let expected = page.rect_of(id).expect("the box was drawn");
    assert_eq!(rect.x, f64::from(expected.x));
    assert_eq!(rect.y, f64::from(expected.y));
    assert_eq!(rect.width, f64::from(expected.width));
    assert_eq!(rect.height, f64::from(expected.height));
    assert!(
        rect.y >= UI_HEIGHT,
        "the overlay is in window coordinates, below the toolbar"
    );
}

/// A page whose one element lays its children into tracks.
struct GridLoader;

impl Loader for GridLoader {
    fn load(&self, _url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            content_type: Some("text/html".to_owned()),
            bytes: b"<style>.g { display: grid; gap: 10px; \
                         grid-template-columns: 100px 100px; }</style>\
                         <div class=g><div>a</div><div>b</div>\
                         <div>c</div><div>d</div></div>\
                         <p>a block, which lays nothing into anything"
                .to_vec(),
            charset: Some("utf-8".to_owned()),
            final_url: "https://grid.example/".to_owned(),
            ..Default::default()
        })
    }
}

/// Choose the first element the document has whose tag is `tag`.
fn choose(browser: &mut Browser, tag: &str) {
    let page = browser.tabs[0].page.as_ref().expect("a page");
    let document = page.document();
    let mut stack = vec![document.root()];
    while let Some(node) = stack.pop() {
        let matches = document.get(node).is_some_and(|node| {
            matches!(&node.data,
                otlyra_dom::NodeData::Element(element)
                    if element.name.local.as_ref() == tag)
        });
        if matches {
            browser.inspector.selected = Some(node);
            return;
        }
        stack.extend(document.children(node));
    }
    panic!("the document has no {tag}");
}

#[test]
fn a_container_that_lays_its_children_into_tracks_gets_the_dashed_overlay() {
    let mut browser = Browser::new(GridLoader);
    browser.navigate("grid.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    choose(&mut browser, "div");
    let chosen = browser.chosen_box().expect("the grid was drawn");
    let tracks = chosen.tracks.expect("a grid has tracks");
    assert!(
        tracks.numbered,
        "a grid names its lines and a flex row does not"
    );

    // Two columns of a hundred with a ten-pixel gutter: three lines, and the
    // far side of the gutter is the same line rather than a fourth.
    let numbered = tracks
        .columns
        .iter()
        .filter(|line| line.number.is_some())
        .count();
    assert_eq!(numbered, 3, "columns: {:?}", tracks.columns);

    // And a block lays nothing into anything, so it has no lines to draw.
    choose(&mut browser, "p");
    let block = browser.chosen_box().expect("the paragraph was drawn");
    assert!(block.tracks.is_none());
}

/// A picture of `bytes` bytes, with no pixels worth looking at.
fn picture(bytes: usize) -> otlyra_gfx::peniko::ImageData {
    let side = 1u32;
    otlyra_gfx::peniko::ImageData {
        data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![0u8; bytes])),
        format: otlyra_gfx::peniko::ImageFormat::Rgba8,
        alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
        width: side,
        height: side,
    }
}

/// The cache keeps what fits and drops what has not been looked at longest.
#[test]
fn the_image_cache_evicts_the_least_recently_used() {
    let mut cache = ImageCache::default();
    let big = IMAGE_CACHE_BUDGET / 2;

    cache.insert("a".to_owned(), picture(big));
    cache.insert("b".to_owned(), picture(big));
    assert!(cache.get("a").is_some(), "both fit");

    // `a` was just used, so `b` is the one that goes.
    cache.insert("c".to_owned(), picture(big));
    assert!(cache.get("b").is_none(), "the older one should have gone");
    assert!(cache.get("a").is_some());
    assert!(cache.get("c").is_some());
    assert!(cache.bytes <= IMAGE_CACHE_BUDGET);
}

/// One larger than the whole budget is not worth emptying the cache for.
#[test]
fn an_oversized_picture_is_not_cached_at_all() {
    let mut cache = ImageCache::default();
    cache.insert("small".to_owned(), picture(1024));
    cache.insert("huge".to_owned(), picture(IMAGE_CACHE_BUDGET + 1));

    assert!(cache.get("huge").is_none());
    assert!(cache.get("small").is_some(), "it emptied the cache anyway");
}

/// A picture that has already been decoded is not fetched again — which is what
/// the cache is for, and is visible in what the loader was asked for.
#[test]
fn a_cached_picture_is_not_fetched_twice() {
    struct PictureLoader {
        requested: Requests,
    }

    impl Loader for PictureLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            self.requested
                .lock()
                .expect("no panic on the fetch thread")
                .push(url.to_owned());
            if url.ends_with(".png") {
                // A one-pixel PNG, so the decoder has something real to do.
                return Ok(Loaded {
                    content_type: Some("image/png".to_owned()),
                    bytes: ONE_PIXEL_PNG.to_vec(),
                    final_url: url.to_owned(),
                    ..Default::default()
                });
            }
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<body><img src=\"/pic.png\"><img src=\"/pic.png\">".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: "https://pictures.example/".to_owned(),
                ..Default::default()
            })
        }
    }

    let requested = Requests::default();
    let mut browser = Browser::new(PictureLoader {
        requested: std::sync::Arc::clone(&requested),
    });

    go(&mut browser, "pictures.example");
    let first = asked_for(&requested);
    assert_eq!(
        first.iter().filter(|url| url.ends_with(".png")).count(),
        1,
        "one address, one fetch, however many elements ask for it"
    );

    go(&mut browser, "pictures.example");
    let second = asked_for(&requested);
    assert_eq!(
        second.iter().filter(|url| url.ends_with(".png")).count(),
        1,
        "the picture was decoded again on the second visit"
    );
}

/// A window that grows past the file its pictures were chosen for asks again.
///
/// The choice among the several a `srcset` offers is made against the window
/// the page loads into, and that window is not the one it stays in. Chosen
/// once and never revisited, a page opened narrow and then widened keeps the
/// small file and draws it stretched.
#[test]
fn a_widened_window_asks_for_a_larger_picture() {
    struct PictureLoader {
        requested: Requests,
    }

    impl Loader for PictureLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            self.requested
                .lock()
                .expect("no panic on the fetch thread")
                .push(url.to_owned());
            if url.ends_with(".png") {
                return Ok(Loaded {
                    content_type: Some("image/png".to_owned()),
                    bytes: ONE_PIXEL_PNG.to_vec(),
                    final_url: url.to_owned(),
                    ..Default::default()
                });
            }
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<body><img sizes=\"100vw\" \
                             srcset=\"/narrow.png 400w, /wide.png 1600w\" src=\"/narrow.png\">"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: "https://pictures.example/".to_owned(),
                ..Default::default()
            })
        }
    }

    let requested = Requests::default();
    let mut browser = Browser::new(PictureLoader {
        requested: std::sync::Arc::clone(&requested),
    });

    let narrow = Viewport::new(400, 600, 1.0);
    browser.set_viewport(narrow);
    go(&mut browser, "pictures.example");

    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, narrow);
    let asked = asked_for(&requested);
    assert!(
        asked.iter().any(|url| url.ends_with("/narrow.png"))
            && !asked.iter().any(|url| url.ends_with("/wide.png")),
        "a narrow window takes the small file: {asked:?}"
    );

    // Wider than the small file can cover, so the element now wants the
    // large one.
    let wide = Viewport::new(1400, 600, 1.0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !asked_for(&requested)
        .iter()
        .any(|url| url.ends_with("/wide.png"))
        && std::time::Instant::now() < deadline
    {
        browser.paint(&mut painter, wide);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert!(
        asked_for(&requested)
            .iter()
            .any(|url| url.ends_with("/wide.png")),
        "the widened window never asked for the larger file: {:?}",
        asked_for(&requested)
    );

    // And it is the picture the element now holds, rather than a fetch that
    // went nowhere.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        browser.paint(&mut painter, wide);
        let held = browser.tabs[0]
            .page
            .as_ref()
            .and_then(|page| {
                let node = otlyra_layout::image_sources(
                    page.document(),
                    otlyra_css::cascade::Viewport::default(),
                )
                .first()?
                .node;
                Some(page.picture_source(node)?.0.to_owned())
            })
            .unwrap_or_default();
        if held.ends_with("/wide.png") || std::time::Instant::now() >= deadline {
            assert!(
                held.ends_with("/wide.png"),
                "the element still holds {held}"
            );
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// A frame takes in whatever has arrived, even when nothing woke the loop.
///
/// The regression this pins: a page asked for before the window exists finishes
/// before there is a waker to be woken by, so that wake is lost. If a frame did
/// not take results in as well, the tab would stay loading — and the spinner
/// would turn for a page that had already arrived.
#[test]
fn a_frame_takes_in_a_load_that_nothing_woke_the_loop_for() {
    let mut browser = browser();
    browser.navigate("example.com");

    // Nothing here pumps but painting, which is the whole point.
    let mut painter = otlyra_gfx::RecordingPainter::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while browser.tabs[0].loading() && std::time::Instant::now() < deadline {
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert!(!browser.tabs[0].loading(), "the tab is still loading");
    assert!(browser.tabs[0].page.is_some());
}

/// Back and forward walk the addresses the tab has been to, and a new tab has
/// nowhere to go in either direction.
#[test]
fn back_and_forward_walk_the_history() {
    let mut browser = browser();
    assert!(!browser.can_go_back() && !browser.can_go_forward());

    browser.navigate("one.example");
    settle(&mut browser);
    assert!(!browser.can_go_back(), "one entry is nowhere to go back to");
    browser.navigate("two.example");
    settle(&mut browser);
    browser.navigate("three.example");
    settle(&mut browser);

    assert!(browser.can_go_back() && !browser.can_go_forward());
    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://two.example/");
    assert!(browser.can_go_forward());

    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://one.example/");
    assert!(!browser.can_go_back());
    browser.go_back();
    settle(&mut browser);
    assert_eq!(
        browser.tabs[0].url, "https://one.example/",
        "and no further"
    );

    browser.go_forward();
    settle(&mut browser);
    browser.go_forward();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://three.example/");
    browser.go_forward();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://three.example/", "nor further");
}

/// Going somewhere new after going back drops what was ahead: those entries
/// describe a future that did not happen.
#[test]
fn navigating_after_going_back_drops_the_forward_entries() {
    let mut browser = browser();
    browser.navigate("one.example");
    settle(&mut browser);
    browser.navigate("two.example");
    settle(&mut browser);
    browser.go_back();
    settle(&mut browser);
    browser.navigate("three.example");
    settle(&mut browser);

    assert!(!browser.can_go_forward());
    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://one.example/");
}

/// A reload is the same place twice, not two places.
#[test]
fn a_reload_adds_no_history_entry() {
    let mut browser = browser();
    browser.navigate("one.example");
    settle(&mut browser);
    browser.navigate("two.example");
    settle(&mut browser);
    browser.reload();
    settle(&mut browser);

    assert!(!browser.can_go_forward());
    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://one.example/");
}

/// One scroll event, and everything it can land on goes the same way.
///
/// The property that was broken: the page added the delta and the browser's
/// own surfaces subtracted it, so a wheel that went down a document went up
/// the settings. Nobody notices which of the two is "right" until they are
/// different, which is why this is asserted rather than commented.
#[test]
fn a_scroll_goes_the_same_way_on_a_document_and_on_a_browser_page() {
    let mut painter = otlyra_gfx::RecordingPainter::new();

    let mut browser = Browser::new(LongLoader);
    browser.navigate("long.example");
    settle(&mut browser);
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    browser.on_event(PlatformEvent::PointerMoved { x: 400.0, y: 400.0 });
    browser.on_event(PlatformEvent::Scroll {
        x: 0.0,
        y: 120.0,
        source: otlyra_platform::ScrollSource::Wheel,
        modifiers: Default::default(),
    });
    let page = browser.tabs[0].page.as_ref().expect("a page").scroll();
    assert!(page > 0.0, "a positive delta goes down the document");

    browser.open_system(SystemPage::Settings);
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    browser.on_event(PlatformEvent::Scroll {
        x: 0.0,
        y: 120.0,
        source: otlyra_platform::ScrollSource::Wheel,
        modifiers: Default::default(),
    });
    assert!(
        browser.settings.settings.scroll > 0.0,
        "and down the browser's own page, by the same event"
    );
}

/// A trackpad's small precise deltas are a distance, not a notch.
#[test]
fn a_trackpad_scrolls_by_what_it_says_rather_than_by_a_notch() {
    let mut browser = Browser::new(LongLoader);
    browser.navigate("long.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    browser.on_event(PlatformEvent::PointerMoved { x: 400.0, y: 400.0 });

    // Three pixels is three pixels. A browser that read this as a notch
    // would jump the page by a wheel's worth for a gesture that moved a
    // finger a hair.
    browser.on_event(PlatformEvent::Scroll {
        x: 0.0,
        y: 3.0,
        source: otlyra_platform::ScrollSource::Trackpad,
        modifiers: Default::default(),
    });
    assert_eq!(browser.tabs[0].page.as_ref().expect("a page").scroll(), 3.0);
}

/// Where the reader had got to comes back with the page, which is the part of
/// a back button people actually notice.
#[test]
fn going_back_restores_where_the_reader_was() {
    let mut browser = Browser::new(LongLoader);
    browser.navigate("long.example");
    settle(&mut browser);
    browser.tabs[0]
        .page
        .as_mut()
        .expect("a page")
        .set_scroll(120.0);

    browser.navigate("long.example/second");
    settle(&mut browser);
    browser.go_back();
    settle(&mut browser);
    assert_eq!(
        browser.tabs[0].page.as_ref().expect("a page").scroll(),
        120.0
    );
}

#[test]
fn the_network_list_holds_every_request_and_what_became_of_it() {
    use crate::fetcher::{ResourceKind, Status};

    let mut browser = Browser::new(SiteLoader::default());
    browser.navigate("https://site.example/");
    settle(&mut browser);
    // A frame, because a stylesheet is asked for while the document is being
    // turned into one.
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    settle(&mut browser);

    let listed: Vec<(&str, ResourceKind)> = browser
        .fetcher
        .exchanges()
        .iter()
        .map(|exchange| (exchange.url.as_str(), exchange.kind))
        .collect();
    assert_eq!(
        listed,
        [
            ("https://site.example/", ResourceKind::Document),
            ("https://site.example/site.css", ResourceKind::Stylesheet),
            ("https://site.example/missing.css", ResourceKind::Stylesheet),
        ],
        "exactly what was asked for, in the order it was asked for"
    );

    // And what became of each: the one that arrived says how much of it
    // there was, and the one that did not says why.
    let by_url = |wanted: &str| {
        browser
            .fetcher
            .exchanges()
            .iter()
            .find(|exchange| exchange.url == wanted)
            .expect("listed above")
            .clone()
    };
    assert!(matches!(
        by_url("https://site.example/site.css").status,
        Status::Ok(bytes) if bytes > 0
    ));
    assert_eq!(
        by_url("https://site.example/missing.css").status,
        Status::Failed("404".to_owned())
    );
    assert!(
        by_url("https://site.example/site.css").took.is_some(),
        "a finished request knows how long the transport took"
    );
}

#[test]
fn the_text_size_preference_is_the_default_a_page_inherits() {
    /// A page that names no size, and one that names its own.
    struct Sized;
    impl Loader for Sized {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            let bytes = if url.contains("named") {
                b"<body><p style=\"font-size: 15px\">text".to_vec()
            } else {
                b"<body><p>text".to_vec()
            };
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes,
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    /// The size the one paragraph was computed at.
    fn paragraph(browser: &mut Browser) -> f32 {
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        let page = browser.active_page().expect("a page");
        let boxes = page.boxes();
        boxes
            .descendants(boxes.root())
            .into_iter()
            .filter_map(|id| boxes.get(id))
            .find(|node| node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "p"))
            .expect("a paragraph")
            .style
            .font_size
    }

    let mut browser = Browser::new(Sized);
    browser.navigate("https://plain.example/");
    settle(&mut browser);
    let ordinary = paragraph(&mut browser);

    browser.settings.settings.text_scale = 200.0;
    let doubled = paragraph(&mut browser);
    assert!(
        (doubled - ordinary * 2.0).abs() < 0.01,
        "a page that names no size inherits the reader's default: \
             {ordinary} became {doubled}"
    );

    // And a page that names one still wins, because this is a default and
    // not an override — which is the part that surprises people, and the
    // part that would be wrong the other way round.
    browser.navigate("https://named.example/");
    settle(&mut browser);
    assert!(
        (paragraph(&mut browser) - 15.0).abs() < 0.01,
        "a page that names its own size keeps it"
    );
}

#[test]
fn the_appearance_preference_is_what_a_page_asks_for() {
    /// A page that draws itself differently in the dark.
    struct Themed;
    impl Loader for Themed {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<style>\
                             p { background: rgb(255, 255, 255) }\
                             @media (prefers-color-scheme: dark) { \
                               p { background: rgb(0, 0, 0) } }\
                             </style><body><p>text"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    /// What the one paragraph is painted behind, after a frame.
    fn background(browser: &mut Browser) -> [u8; 4] {
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        let page = browser.active_page().expect("a page");
        let boxes = page.boxes();
        let colour = boxes
            .descendants(boxes.root())
            .into_iter()
            .filter_map(|id| boxes.get(id))
            .find(|node| node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "p"))
            .expect("a paragraph")
            .style
            .background_color;
        colour.to_rgba8().to_u8_array()
    }

    let mut browser = Browser::new(Themed);
    browser.navigate("https://themed.example/");
    settle(&mut browser);
    assert_eq!(
        background(&mut browser),
        [255, 255, 255, 255],
        "the preference starts at light"
    );

    browser.settings.settings.appearance = crate::settings::Appearance::Dark;
    assert_eq!(
        background(&mut browser),
        [0, 0, 0, 255],
        "and the page follows it"
    );
}

#[test]
fn turning_pictures_off_means_none_are_asked_for() {
    /// A page with a picture in it, and a log of everything asked for.
    #[derive(Default)]
    struct Pictures {
        requested: Requests,
    }

    impl Loader for Pictures {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            self.requested
                .lock()
                .expect("no panic on the fetch thread")
                .push(url.to_owned());
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<body><img src=picture.png><p>text".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: "https://pictures.example/".to_owned(),
                ..Default::default()
            })
        }
    }

    let asked = |browser: &mut Browser, requested: &Requests| {
        browser.navigate("https://pictures.example/");
        settle(browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        settle(browser);
        requested
            .lock()
            .expect("no panic on the fetch thread")
            .clone()
    };

    let loader = Pictures::default();
    let requested = std::sync::Arc::clone(&loader.requested);
    let mut browser = Browser::new(loader);
    let with = asked(&mut browser, &requested);
    assert!(
        with.iter().any(|url| url.contains("picture.png")),
        "the picture is asked for by default: {with:?}"
    );

    let loader = Pictures::default();
    let requested = std::sync::Arc::clone(&loader.requested);
    let mut browser = Browser::new(loader);
    browser.settings.settings.load_images = false;
    let without = asked(&mut browser, &requested);
    // Refused before the request rather than after it: a picture fetched and
    // then not shown has already cost the reader their bandwidth and told
    // the server they were here.
    assert!(
        !without.iter().any(|url| url.contains("picture.png")),
        "and not asked for at all when the preference says so: {without:?}"
    );
    assert!(
        without.iter().any(|url| url.contains("pictures.example")),
        "the page itself still loads"
    );
}

#[test]
fn two_tabs_keep_their_own_contents_across_a_switch() {
    let mut browser = Browser::new(LongLoader);
    browser.navigate("long.example");
    settle(&mut browser);

    browser.new_tab();
    browser.open_system(SystemPage::Settings);
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));

    // A browser page is a place a tab can be rather than a mode the window
    // is in, so the other tab is still on its document.
    browser.select_tab(0);
    assert_eq!(browser.system_page(), None);
    assert!(browser.tabs[0].page.is_some());

    browser.select_tab(1);
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    assert!(browser.tabs[1].page.is_none());
}

#[test]
fn the_history_walks_through_a_browser_page_like_any_other() {
    let mut browser = Browser::new(LongLoader);
    browser.navigate("long.example");
    settle(&mut browser);
    let document = browser.tabs[0].url.clone();

    // Left somewhere down the page, which is the thing going back has to
    // bring back along with the document.
    browser.tabs[0]
        .page
        .as_mut()
        .expect("a page")
        .set_scroll(120.0);

    browser.open_system(SystemPage::Settings);
    assert_eq!(browser.tabs[0].url, "about:settings");

    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, document);
    assert_eq!(
        browser.tabs[0].page.as_ref().expect("a page").scroll(),
        120.0,
        "and at the place it was left at"
    );

    browser.go_forward();
    settle(&mut browser);
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
}

#[test]
fn done_goes_back_rather_than_wiping_the_tab() {
    let mut browser = Browser::new(LongLoader);
    browser.navigate("long.example");
    settle(&mut browser);
    let document = browser.tabs[0].url.clone();
    browser.open_system(SystemPage::Settings);

    browser.handle_settings_action(&settings::Action::Close);
    settle(&mut browser);
    assert_eq!(
        browser.tabs[0].url, document,
        "the reader goes back to what they were reading"
    );

    // And with nowhere to go back to, an empty tab rather than a settings
    // page nobody can leave.
    let mut fresh = Browser::new(LongLoader);
    fresh.open_system(SystemPage::Settings);
    fresh.handle_settings_action(&settings::Action::Close);
    assert_eq!(fresh.system_page(), None);
    assert!(fresh.tabs[0].url.is_empty());
}

#[test]
fn a_browser_page_is_left_and_returned_to_where_the_reader_was() {
    let mut browser = Browser::new(LongLoader);
    browser.open_system(SystemPage::Settings);
    // Drawn, because how far a surface can scroll is only known once it has
    // been: the same rule the panel and the page both follow.
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
    browser.settings.scroll_by(140.0);
    let left_at = browser.settings.settings.scroll;
    assert!(left_at > 0.0, "the settings scrolled at all");

    browser.navigate("long.example");
    settle(&mut browser);

    // Scrambled while nobody is looking at it, because the surface is the
    // browser's and another tab may have used it in between. What brings the
    // reader back to where they were is the history entry, not the surface
    // happening to still hold the number.
    browser.settings.settings.scroll = 999.0;

    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    assert_eq!(
        browser.settings.settings.scroll, left_at,
        "and a browser page is returned to where the reader was, like any other"
    );
}

#[test]
fn a_browser_page_is_reached_by_typing_its_address() {
    let mut browser = Browser::new(LongLoader);
    // Every navigation goes through one place, so the address bar reaches
    // `about:` the same way the menu does.
    browser.navigate("about:settings");
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    assert_eq!(browser.ui.address.text(), "about:settings");
}

/// A screen reader's press on the page is a press: it ticks the box, and the
/// tree says so afterwards.
#[test]
fn a_readers_press_ticks_a_checkbox_on_the_page() {
    struct FormPage;

    impl Loader for FormPage {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<body><label><input type=checkbox> Send me post</label>".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(FormPage);
    go(&mut browser, "https://site.example/");
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(1000, 700, 1.0));

    let update = browser.accessibility().expect("a tree");
    let (id, node) = update
        .nodes
        .iter()
        .find(|(_, node)| node.role() == otlyra_platform::accesskit::Role::CheckBox)
        .expect("the checkbox");
    assert_eq!(
        node.toggled(),
        Some(otlyra_platform::accesskit::Toggled::False)
    );

    browser.on_event(PlatformEvent::AccessibilityRequest {
        node: *id,
        action: otlyra_platform::AccessibilityAction::Activate,
    });
    browser.paint(&mut painter, Viewport::new(1000, 700, 1.0));

    let update = browser.accessibility().expect("a tree");
    let (_, node) = update
        .nodes
        .iter()
        .find(|(_, node)| node.role() == otlyra_platform::accesskit::Role::CheckBox)
        .expect("the checkbox");
    assert_eq!(
        node.toggled(),
        Some(otlyra_platform::accesskit::Toggled::True),
        "the press was swallowed"
    );
}

/// And a press on a button sends the form behind it, which is the whole of
/// what pressing one means without a script.
#[test]
fn a_readers_press_sends_the_form() {
    struct SearchPage;

    impl Loader for SearchPage {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<body><form action=/search><input name=q value=cats>\
                      <input type=submit value=Go></form>"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(SearchPage);
    go(&mut browser, "https://site.example/");
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(1000, 700, 1.0));

    let update = browser.accessibility().expect("a tree");
    let (id, _) = update
        .nodes
        .iter()
        .find(|(id, node)| {
            crate::a11y::described_index(*id).is_none()
                && node.role() == otlyra_platform::accesskit::Role::Button
        })
        .expect("the button");

    browser.on_event(PlatformEvent::AccessibilityRequest {
        node: *id,
        action: otlyra_platform::AccessibilityAction::Activate,
    });
    settle(&mut browser);
    assert_eq!(
        browser.tabs[browser.active].url,
        "https://site.example/search?q=cats"
    );
}

/// A form that posts sends its body, and the answer becomes the page.
///
/// Everything before the request is tested where it is built; what this holds
/// is the last stretch, which was the missing one: the method, the body and the
/// type reach the transport, and the page they come back with is the tab's.
#[test]
fn a_form_that_posts_sends_its_body() {
    /// Every request the browser made, with whatever body it carried.
    type Sent = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<Body>)>>>;

    struct PostLoader {
        sent: Sent,
    }

    impl Loader for PostLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            self.send(url, None)
        }

        fn send(&self, url: &str, body: Option<Body>) -> Result<Loaded, String> {
            self.sent
                .lock()
                .expect("no panic on the fetch thread")
                .push((url.to_owned(), body.clone()));
            let bytes = if body.is_some() {
                b"<body><p>saved".to_vec()
            } else {
                b"<body><form method=post action=/save>\
                      <input name=who value=Ada><input type=submit value=Go></form>"
                    .to_vec()
            };
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes,
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let sent = Sent::default();
    let mut browser = Browser::new(PostLoader {
        sent: std::sync::Arc::clone(&sent),
    });
    go(&mut browser, "https://site.example/");

    // Pressed where it was drawn, which needs a frame: the button's rectangle
    // is the last layout's, like every other press.
    let active = browser.active;
    let page = browser.tabs[active].page.as_mut().expect("a page");
    page.build_display_list(&mut TextEngine::isolated(), 800.0, 600.0, 0.0);
    let boxes = page.boxes();
    let button = boxes
        .descendants(boxes.root())
        .into_iter()
        .filter(|&id| boxes.node(id).control.is_some())
        .nth(1)
        .expect("the button");
    let rect = page.rect_of(button).expect("a rectangle");
    let (x, y) = (
        f64::from(rect.x + rect.width / 2.0),
        f64::from(rect.y + rect.height / 2.0),
    );
    page.pointer_pressed(x, y);
    page.pointer_released(x, y);
    browser.follow_submission();
    settle(&mut browser);

    let sent = sent.lock().expect("no panic on the fetch thread").clone();
    let (url, body) = sent.last().expect("the form was sent").clone();
    assert_eq!(url, "https://site.example/save");
    let body = body.expect("a POST carries a body");
    assert_eq!(body.content_type, "application/x-www-form-urlencoded");
    assert_eq!(body.bytes, b"who=Ada");
    assert_eq!(
        browser.tabs[browser.active].url, "https://site.example/save",
        "and the tab is where the form sent it"
    );
}

/// A frame must not wait on a request only a driver can release.
///
/// The deadlock this guards: the gate holds a picture the page asked for,
/// `prepare_frame` waits for it, and the command that would let it go arrives
/// on the thread now sitting inside `prepare_frame`. Waiting there is waiting
/// for oneself, and it used to cost a full load timeout every frame.
#[test]
fn a_frame_does_not_wait_for_a_picture_a_driver_is_holding() {
    struct WithPicture;

    impl Loader for WithPicture {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<body><img src=held.png><p>text".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(WithPicture);
    browser.hold_requests(Some(Box::new(|url: &str| url.ends_with(".png"))));
    go(&mut browser, "https://pictures.example/");

    let started = std::time::Instant::now();
    browser.prepare_frame(
        Viewport::new(800, 600, 1.0),
        std::time::Duration::from_secs(10),
    );
    let took = started.elapsed();

    assert!(
        !browser.held().is_empty(),
        "the picture should have been held, or this proves nothing"
    );
    assert!(
        took < std::time::Duration::from_secs(2),
        "the frame waited {took:?} for a request only a driver could release"
    );
}

/// A site whose CSS lives in a file next to the page.
#[derive(Default)]
struct SiteLoader {
    requested: Requests,
}

impl Loader for SiteLoader {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        self.requested
            .lock()
            .expect("no panic on the fetch thread")
            .push(url.to_owned());
        match url {
            "https://site.example/site.css" => Ok(Loaded {
                content_type: Some("text/css".to_owned()),
                bytes: b"p { color: rgb(0, 128, 0) }".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            }),
            "https://site.example/missing.css" => Err("404".to_owned()),
            _ => Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<link rel=stylesheet href=site.css>\
                      <link rel=stylesheet href=missing.css>\
                      <link rel=icon href=favicon.ico>\
                      <body><p>text"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: "https://site.example/".to_owned(),
                ..Default::default()
            }),
        }
    }
}

/// A linked stylesheet is fetched against the page's own address, and only the
/// links that are stylesheets are fetched at all.
#[test]
fn navigation_fetches_the_stylesheets_the_page_links() {
    let requested = Requests::default();
    let mut browser = Browser::new(SiteLoader {
        requested: std::sync::Arc::clone(&requested),
    });
    go(&mut browser, "site.example");

    // Sorted, because the fetch pool serves several at once and the order two
    // stylesheets come back in is not the browser's to promise.
    let mut asked = asked_for(&requested);
    asked.sort();
    assert_eq!(
        asked,
        vec![
            "https://site.example/missing.css".to_owned(),
            "https://site.example/site.css".to_owned(),
            "site.example".to_owned(),
        ],
        "the icon is not a stylesheet and is not fetched"
    );

    let active = browser.active;
    let page = browser.tabs[active].page.as_mut().expect("a page");
    // The cascade runs on the way to a frame, so ask for one.
    page.build_display_list(&mut TextEngine::isolated(), 800.0, 600.0, 0.0);

    let boxes = page.boxes();
    let coloured = boxes
        .descendants(boxes.root())
        .into_iter()
        .any(|id| boxes.node(id).style.color == otlyra_gfx::peniko::Color::from_rgb8(0, 128, 0));
    assert!(coloured, "the fetched sheet reached the box tree");
}

/// A document is not drawn before the stylesheet it links.
///
/// It was, and what a reader saw on any page with an external sheet was the
/// author's markup in none of the author's design, replaced a moment later
/// by the real thing. That reads as the CSS arriving slowly; it is the frame
/// arriving early. A picture is not on the list — nothing holds a page back
/// for a photograph.
#[test]
fn a_document_is_not_drawn_before_the_stylesheet_it_links() {
    /// A loader that answers the document and never the sheet, which is what
    /// a slow server looks like from here.
    struct SlowSheet;

    impl Loader for SlowSheet {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            if url.ends_with(".css") {
                // Long enough that the frame below is built while it is still
                // outstanding, short enough that the test does not hang.
                std::thread::sleep(std::time::Duration::from_millis(400));
                return Ok(Loaded {
                    content_type: Some("text/css".to_owned()),
                    bytes: b"p { color: #008000 }".to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                });
            }
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<title>T</title><link rel=stylesheet href=/s.css><body><p>text".to_vec(),
                charset: Some("utf-8".to_owned()),
                // Absolute, so the `<link>` beside it has something to
                // resolve against.
                final_url: format!("https://{url}/"),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(SlowSheet);
    browser.navigate("slow.example");
    // The document itself is in; the sheet is not.
    browser.wait_for_load(std::time::Duration::from_millis(120));
    let active = browser.active;
    assert!(
        browser.blocked_on_style(active),
        "the sheet is still outstanding"
    );

    // The document's one paragraph is four letters, and nothing the chrome
    // or the blank page draws is a run of exactly four. Counting runs alone
    // would prove nothing: the blank page draws a line of its own, so the
    // number is the same either way and only *which* run is there differs.
    let paragraph = |browser: &mut Browser| {
        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, Viewport::new(800, 600, 1.0));
        target.ops().iter().any(
            |op| matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { glyphs, .. } if glyphs.len() == 4),
        )
    };

    assert!(
        !paragraph(&mut browser),
        "the document was drawn before its stylesheet arrived"
    );

    browser.wait_for_load(std::time::Duration::from_secs(5));
    assert!(!browser.blocked_on_style(active), "the sheet arrived");
    assert!(
        paragraph(&mut browser),
        "and it was drawn once the stylesheet was in"
    );
}

/// A zoomed page is laid out in fewer CSS pixels and drawn back up to fill
/// the window — it reflows, rather than being magnified.
///
/// That is the whole difference between a page zoom and a picture of a page
/// scaled up, and it is what a reader wants: bigger text that still uses the
/// window it is in.
#[test]
fn a_zoomed_page_reflows_rather_than_being_magnified() {
    struct Prose;

    impl Loader for Prose {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<title>T</title><body style='margin:0'><p>one two three four \
                             five six seven eight nine ten eleven twelve thirteen fourteen \
                             fifteen sixteen seventeen eighteen nineteen twenty"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: format!("https://{url}/"),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(Prose);
    go(&mut browser, "prose.example");

    let runs = |browser: &mut Browser| {
        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, Viewport::new(800, 600, 1.0));
        target
            .ops()
            .iter()
            .filter(|op| matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { .. }))
            .count()
    };

    assert_eq!(browser.zoom(), 1.0, "a page opens at its own size");
    let plain = runs(&mut browser);

    browser.set_zoom(2.0);
    assert_eq!(browser.zoom(), 2.0);
    let zoomed = runs(&mut browser);
    assert!(
        zoomed > plain,
        "the paragraph broke into more lines in half the pixels: {plain} then {zoomed}"
    );

    // And back, exactly: a reader who undoes a zoom gets the page they had.
    browser.set_zoom(1.0);
    assert_eq!(runs(&mut browser), plain);

    // The range is every browser's, and a factor outside it is brought back
    // rather than refused — a control that silently does nothing is worse
    // than one that stops.
    browser.set_zoom(50.0);
    assert_eq!(browser.zoom(), 5.0);
    browser.set_zoom(0.01);
    assert_eq!(browser.zoom(), 0.25);
}

/// The zoom is reached the three ways a reader reaches it, and lands on the
/// stops a menu can name.
#[test]
fn the_zoom_steps_along_a_ladder_from_the_keyboard_the_menu_and_the_wheel() {
    let accelerator = Modifiers {
        command: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        ..Modifiers::default()
    };
    let mut browser = browser();
    go(&mut browser, "example.com");

    let press = |browser: &mut Browser, character: char| {
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Character(character),
            modifiers: accelerator,
        });
    };

    press(&mut browser, '=');
    assert_eq!(browser.zoom(), 1.1, "one stop up, not one and a bit");
    press(&mut browser, '=');
    assert_eq!(browser.zoom(), 1.25);
    press(&mut browser, '-');
    assert_eq!(browser.zoom(), 1.1);
    press(&mut browser, '0');
    assert_eq!(browser.zoom(), 1.0, "and back to the page's own size");

    // The menu reaches the same ladder.
    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ZoomIn.id(),
    ));
    assert_eq!(browser.zoom(), 1.1);
    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ActualSize.id(),
    ));
    assert_eq!(browser.zoom(), 1.0);

    // And the wheel, but only with the modifier held: without it the page
    // scrolls, which is what a wheel is for.
    let wheel = |browser: &mut Browser, y: f64, modifiers: Modifiers| {
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y,
            source: otlyra_platform::ScrollSource::Wheel,
            modifiers,
        });
    };
    wheel(&mut browser, -40.0, accelerator);
    assert_eq!(browser.zoom(), 1.1, "away from the reader is larger");
    wheel(&mut browser, 40.0, accelerator);
    assert_eq!(browser.zoom(), 1.0);
    wheel(&mut browser, -40.0, Modifiers::default());
    assert_eq!(
        browser.zoom(),
        1.0,
        "a bare wheel scrolls and does not zoom"
    );

    // The ends of the ladder are ends, not a wrap: a reader holding the key
    // down stops at the largest rather than starting again at the smallest.
    for _ in 0..40 {
        press(&mut browser, '=');
    }
    assert_eq!(browser.zoom(), 5.0);
    for _ in 0..40 {
        press(&mut browser, '-');
    }
    assert_eq!(browser.zoom(), 0.25);
}

/// A zoom is remembered against the site, and only against the site the
/// reader set it on.
#[test]
fn a_zoom_belongs_to_the_site_it_was_set_on() {
    let mut browser = browser();
    go(&mut browser, "one.example");
    browser.step_zoom(ZoomStep::In);
    assert_eq!(browser.zoom(), 1.1);

    // Another site is another zoom, which is the whole reason this is not
    // one number for the browser.
    go(&mut browser, "two.example");
    assert_eq!(browser.zoom(), 1.0, "the next site is left alone");

    go(&mut browser, "one.example");
    assert_eq!(browser.zoom(), 1.1, "and the first is as it was left");

    // Every page of a site is the site: a zoom set on one page is the zoom
    // on the next.
    go(&mut browser, "https://one.example/deep/page");
    assert_eq!(browser.zoom(), 1.1);

    // Back to its own size and the site stops being one this browser knows
    // anything about — a preferences file should not carry a line for every
    // place anyone has ever been.
    browser.step_zoom(ZoomStep::Reset);
    assert!(browser.settings.settings.zoom.is_empty());
}

/// A zoom keeps the reader where they were reading.
///
/// The scroll is in the page's own pixels and survives, but the content
/// around it is a different height once the lines have broken elsewhere —
/// so the same offset points at different words. What is held is the place
/// in the text at the top of the window.
#[test]
fn a_zoom_keeps_the_reader_where_they_were_reading() {
    struct LongPage;

    impl Loader for LongPage {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            // Long enough that a tenth off the width breaks them
            // differently, which is what makes the offset in pixels stop
            // meaning what it meant.
            let paragraphs = (0..60)
                .map(|n| {
                    format!(
                        "<p>paragraph number {n} with a great many words in it so that \
                             taking a tenth off the width it is laid out in breaks its lines \
                             somewhere else entirely and the pixels stop lining up</p>"
                    )
                })
                .collect::<String>();
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: format!("<title>T</title><body style='margin:0'>{paragraphs}").into_bytes(),
                charset: Some("utf-8".to_owned()),
                final_url: format!("https://{url}/"),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(LongPage);
    go(&mut browser, "long.example");
    let draw = |browser: &mut Browser| {
        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, Viewport::new(800, 600, 1.0));
    };
    draw(&mut browser);

    // Half way down, and read what is at the top of the window.
    // What is at the top of the window: `select_word_at` takes a window
    // point and a top inset and adds the scroll itself, so the top of the
    // window is `y == top`.
    let words = |browser: &mut Browser| {
        let page = browser.tabs[browser.active].page.as_mut().expect("a page");
        page.select_word_at(40.0, 0.0, 0.0);
        page.selected_text()
    };
    if let Some(page) = browser.tabs[browser.active].page.as_mut() {
        page.set_scroll(700.0);
    }
    draw(&mut browser);
    let before = words(&mut browser);
    assert!(before.is_some(), "there are words at the top of the window");

    browser.step_zoom(ZoomStep::In);
    draw(&mut browser);
    let after = browser.tabs[browser.active]
        .page
        .as_ref()
        .expect("a page")
        .scroll();
    assert_ne!(
        after, 700.0,
        "the page moved to keep the reader's place rather than staying at an \
             offset that now points at other words"
    );
    assert_eq!(
        words(&mut browser),
        before,
        "and the same words are at the top of the window"
    );
}

/// A press lands where the reader aimed it, whatever the zoom.
///
/// The page is laid out in its own pixels, so a pointer arriving in the
/// window's has to be converted — and a press answered in the coordinate
/// system it did not land in is a link that opens when the pointer was
/// somewhere else.
#[test]
fn a_press_on_a_zoomed_page_lands_where_it_was_aimed() {
    let mut browser = browser();
    go(&mut browser, "example.com");

    let draw = |browser: &mut Browser| {
        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, Viewport::new(800, 600, 1.0));
    };
    let hit = |browser: &Browser, x: f64, y: f64| {
        let (x, y) = browser.in_page(x, y);
        browser.tabs[browser.active]
            .page
            .as_ref()
            .expect("a page")
            .box_at(x, y)
    };

    draw(&mut browser);
    // Thirty of the page's own pixels into its content, which unzoomed is
    // thirty of the window's below the chrome.
    let plain = hit(&mut browser, 30.0, UI_HEIGHT + 30.0);
    assert!(
        plain.is_some(),
        "the paragraph is under that point unzoomed"
    );

    browser.set_zoom(2.0);
    draw(&mut browser);
    // The same thirty page pixels, drawn twice as large: twice as far into
    // the window, and the chrome's own inset is not doubled with them.
    assert_eq!(
        hit(&mut browser, 60.0, UI_HEIGHT + 60.0),
        plain,
        "the same place in the page, aimed at where it is now drawn"
    );
}

/// A sheet written for another medium holds nothing back.
///
/// The rule that stops a frame is *this page cannot be drawn right yet*, and
/// a print-only sheet is never going to draw any of it. Holding the screen
/// for one is holding it for nothing.
#[test]
fn a_print_stylesheet_does_not_hold_the_screen() {
    struct PrintSheet;

    impl Loader for PrintSheet {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            if url.ends_with(".css") {
                std::thread::sleep(std::time::Duration::from_millis(400));
                return Ok(Loaded {
                    content_type: Some("text/css".to_owned()),
                    bytes: b"p { color: #008000 }".to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                });
            }
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes:
                    b"<title>T</title><link rel=stylesheet media=print href=/p.css><body><p>text"
                        .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: format!("https://{url}/"),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(PrintSheet);
    browser.navigate("print.example");
    browser.wait_for_load(std::time::Duration::from_millis(120));
    let active = browser.active;
    assert!(
        !browser.blocked_on_style(active),
        "a print-only sheet held the screen"
    );

    let mut target = otlyra_gfx::RecordingPainter::default();
    browser.paint(&mut target, Viewport::new(800, 600, 1.0));
    assert!(
        target.ops().iter().any(|op| {
            matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { glyphs, .. } if glyphs.len() == 4)
        }),
        "the document was drawn while the print sheet was still coming"
    );
}

/// A page from the network asking for a stylesheet on disk is the rule that
/// keeps a web page out of the filesystem, and it holds for subresources and
/// not only for navigation.
#[test]
fn a_web_page_may_not_link_a_stylesheet_on_disk() {
    struct DiskLoader;

    impl Loader for DiskLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            assert!(
                !url.starts_with("file:"),
                "the loader must never be asked for {url}"
            );
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<link rel=stylesheet href=\"file:///etc/theme.css\"><body><p>x".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: "https://site.example/".to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(DiskLoader);
    browser.navigate("site.example");
    settle(&mut browser);
    assert!(browser.tabs[browser.active].page.is_some());
}

/// Where the link's text was actually painted, taken from the page's own
/// targets rather than guessed.
fn link_position(browser: &Browser) -> (f64, f64) {
    let page = browser.tabs[browser.active].page.as_ref().expect("page");
    let mut x = 0.0;
    let mut y = 0.0;
    for offset in 0..2000 {
        let candidate_x = 4.0 + f64::from(offset);
        let candidate_y = UI_HEIGHT + 30.0;
        if page.link_at(candidate_x, candidate_y).is_some() {
            x = candidate_x;
            y = candidate_y;
            break;
        }
    }
    assert!(x > 0.0, "the link should be somewhere on the first line");
    (x, y)
}

#[test]
fn reloading_fetches_the_same_address_again() {
    let (mut browser, requested) = browser_with_log();
    type_url(&mut browser, "example.com");
    browser.reload();
    settle(&mut browser);

    assert_eq!(
        asked_for(&requested),
        ["example.com", "https://example.com/"],
        "the reload asks for where the first load ended up"
    );
}

#[test]
fn stopping_a_load_rejects_its_late_document() {
    struct SlowSecondPage;

    impl Loader for SlowSecondPage {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            if url.contains("second") {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let title = if url.contains("second") {
                "Second"
            } else {
                "First"
            };
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: format!("<title>{title}</title><body>{title}").into_bytes(),
                charset: Some("utf-8".to_owned()),
                final_url: format!("https://{url}/"),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(SlowSecondPage);
    go(&mut browser, "first.example");
    browser.navigate("second.example");
    assert!(browser.tabs[0].loading());

    browser.stop();
    assert!(
        !browser.tabs[0].loading(),
        "the spinner should stop at once"
    );

    std::thread::sleep(std::time::Duration::from_millis(75));
    browser.pump();
    let page = browser.tabs[0]
        .page
        .as_ref()
        .expect("the first page remains");
    assert_eq!(
        crate::page::title_of(page.document()).as_deref(),
        Some("First"),
        "the cancelled navigation arrived late and replaced the page"
    );
}

/// A reload keeps your place. For a page you are editing that is the whole
/// value of the key.
#[test]
fn reloading_keeps_the_scroll_position() {
    let mut browser = Browser::new(LongLoader);
    browser.navigate("long.example");
    settle(&mut browser);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

    browser.ui.pointer_moved(400.0, 400.0, &mut browser.text);
    browser.on_event(PlatformEvent::Scroll {
        x: 0.0,
        y: 200.0,
        source: otlyra_platform::ScrollSource::Wheel,
        modifiers: Default::default(),
    });
    let scrolled = browser.tabs[0].page.as_ref().expect("page").scroll();
    assert!(scrolled > 0.0);

    browser.reload();
    settle(&mut browser);
    assert_eq!(
        browser.tabs[0].page.as_ref().expect("page").scroll(),
        scrolled
    );
}

#[test]
fn reloading_a_blank_tab_does_nothing() {
    let (mut browser, requested) = browser_with_log();
    browser.reload();
    settle(&mut browser);
    assert!(asked_for(&requested).is_empty());
}

/// §14's rule: a page from the internet must never be able to open a file.
#[test]
fn a_web_page_may_not_navigate_to_a_file_url() {
    let (mut browser, requested) = browser_with_log();
    type_url(&mut browser, "example.com");
    browser.navigate_from("file:///etc/passwd", false);
    settle(&mut browser);

    assert_eq!(browser.tabs[0].url, "https://example.com/");
    assert!(
        browser.tabs[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Refused"))
    );
    assert_eq!(
        asked_for(&requested),
        ["example.com"],
        "the loader is never even asked"
    );
}

#[test]
fn the_user_may_open_a_file_url_and_so_may_a_local_page() {
    let (mut browser, requested) = browser_with_log();
    type_url(&mut browser, "file:///tmp/one.html");
    assert_eq!(asked_for(&requested).len(), 1);

    browser.navigate_from("file:///tmp/two.html", false);
    settle(&mut browser);
    assert_eq!(
        asked_for(&requested).len(),
        2,
        "a local page's own link is allowed"
    );
}

/// A page long enough to scroll.
struct LongLoader;

impl Loader for LongLoader {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        let body = "<title>Long</title><body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
        Ok(Loaded {
            content_type: Some("text/html".to_owned()),
            bytes: body.into_bytes(),
            charset: Some("utf-8".to_owned()),
            // A transport hands back the address it actually reached, and an
            // address that already has a scheme is one it reached as given.
            // Prepending one unconditionally made a fake that a second visit
            // to the same page — which is what going back is — turned into
            // `https://https://…`, and only the browser looked wrong.
            final_url: if url.contains("://") {
                url.to_owned()
            } else {
                format!("https://{url}/")
            },
            ..Default::default()
        })
    }
}

/// The content version of one layer in a composed scene.
fn epoch_of(scene: &Scene, id: u64) -> u64 {
    scene
        .layers
        .iter()
        .find(|layer| layer.id == LayerId(id))
        .unwrap_or_else(|| panic!("layer {id} present"))
        .epoch
}

#[test]
fn an_unchanged_frame_composes_to_the_same_layer_epochs() {
    let mut browser = Browser::new(LongLoader);
    go(&mut browser, "long.example");
    let viewport = Viewport::new(1024, 768, 2.0);

    // The first frame settles the caches; two more are what a no-op yields.
    let _ = browser.compose(viewport).expect("the interface composes");
    let before = browser.compose(viewport).expect("the interface composes");
    let after = browser.compose(viewport).expect("the interface composes");

    let ids: Vec<_> = before.layers.iter().map(|layer| layer.id).collect();
    assert!(ids.contains(&LayerId(LAYER_PAGE)), "a page layer");
    assert!(ids.contains(&LayerId(LAYER_CHROME)), "a chrome layer");
    assert_eq!(before.layers.len(), after.layers.len());
    for (b, a) in before.layers.iter().zip(after.layers.iter()) {
        assert_eq!(b.id, a.id);
        assert_eq!(b.rect, a.rect);
        assert_eq!(
            b.epoch, a.epoch,
            "layer {:?} is unchanged between two no-op frames",
            b.id
        );
    }
}

#[test]
fn an_open_menu_expands_the_composited_chrome_layer() {
    let mut browser = Browser::new(LongLoader);
    let viewport = Viewport::new(2048, 1536, 2.0);
    let closed = browser.compose(viewport).expect("the interface composes");
    let closed_chrome = closed
        .layers
        .iter()
        .find(|layer| layer.id == LayerId(LAYER_CHROME))
        .expect("the chrome layer");
    assert_eq!(
        closed_chrome.rect.height,
        (UI_HEIGHT * viewport.scale_factor) as u32
    );

    // The real event route uses logical pointer coordinates at every scale.
    browser.handle_event(PlatformEvent::PointerMoved {
        x: viewport.logical_width() - 22.0,
        y: UI_HEIGHT - 21.0,
    });
    browser.handle_event(PlatformEvent::PointerPressed { clicks: 1 });
    assert!(browser.ui.menu_open(), "the cogwheel opened the menu");

    let open = browser.compose(viewport).expect("the interface composes");
    let open_chrome = open
        .layers
        .iter()
        .find(|layer| layer.id == LayerId(LAYER_CHROME))
        .expect("the chrome layer");
    assert_eq!(
        open_chrome.rect.height, viewport.height,
        "the popup must not be clipped at the toolbar"
    );
}

/// The display list of one layer in a composed scene.
fn list_of(scene: &Scene, id: u64) -> Arc<otlyra_gfx::DisplayList> {
    Arc::clone(
        &scene
            .layers
            .iter()
            .find(|layer| layer.id == LayerId(id))
            .unwrap_or_else(|| panic!("layer {id} present"))
            .list,
    )
}

#[test]
fn an_unchanged_layer_hands_back_the_device_list_it_handed_back_before() {
    let mut browser = Browser::new(LongLoader);
    go(&mut browser, "long.example");
    browser.inspector.open = true;
    let viewport = Viewport::new(1024, 768, 2.0);

    // Two frames after the caches have settled. Nothing moved between them, so
    // no layer may be built again, cloned, or scaled to device pixels again:
    // pointer identity is the only way to say that, because an equal list
    // built twice is exactly the work this is here to prevent.
    let _ = browser.compose(viewport).expect("the interface composes");
    let before = browser.compose(viewport).expect("the interface composes");
    let after = browser.compose(viewport).expect("the interface composes");

    for id in [LAYER_PAGE, LAYER_CHROME, LAYER_INSPECTOR] {
        assert!(
            Arc::ptr_eq(&list_of(&before, id), &list_of(&after, id)),
            "layer {id} was rebuilt or re-scaled for a frame that changed nothing"
        );
    }
}

#[test]
fn a_changed_scale_scales_the_interface_again() {
    let mut browser = Browser::new(LongLoader);
    go(&mut browser, "long.example");

    let one = browser
        .compose(Viewport::new(1024, 768, 1.0))
        .expect("the interface composes");
    let chrome_at_one = list_of(&one, LAYER_CHROME);
    let two = browser
        .compose(Viewport::new(2048, 1536, 2.0))
        .expect("the interface composes");

    // The logical list is the same one — nothing in the interface changed —
    // but the device list it is scaled into is not, or the toolbar would be
    // drawn at half size on a retina display.
    assert!(!Arc::ptr_eq(&chrome_at_one, &list_of(&two, LAYER_CHROME)));
}

#[test]
fn scrolling_the_page_moves_its_layer_epoch_and_leaves_the_chrome_alone() {
    let mut browser = Browser::new(LongLoader);
    go(&mut browser, "long.example");
    let viewport = Viewport::new(1024, 768, 2.0);

    let _ = browser.compose(viewport).expect("the interface composes");
    let before = browser.compose(viewport).expect("the interface composes");

    // Scroll the long page. Only the page's own list is rebuilt; the tab strip
    // and toolbar are drawing nothing new.
    browser.tabs[browser.active]
        .page
        .as_mut()
        .expect("a loaded page")
        .scroll_by(300.0);
    let after = browser.compose(viewport).expect("the interface composes");

    assert_ne!(
        epoch_of(&before, LAYER_PAGE),
        epoch_of(&after, LAYER_PAGE),
        "the page scrolled, so its layer must be re-rasterized"
    );
    assert_eq!(
        epoch_of(&before, LAYER_CHROME),
        epoch_of(&after, LAYER_CHROME),
        "the chrome did not change, so the compositor leaves it untouched"
    );
}

#[test]
fn a_press_on_the_page_blurs_the_address_field() {
    let mut browser = Browser::new(LongLoader);
    go(&mut browser, "long.example");
    // A frame first: the field to focus and the layout to press against are
    // both things the last frame drew.
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(1024, 768, 1.0));

    browser.ui.focus_address();
    assert!(browser.ui.address_focused(), "the address starts focused");

    browser.on_event(PlatformEvent::PointerMoved { x: 500.0, y: 400.0 });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    assert!(
        !browser.ui.address_focused(),
        "a press on the page takes the focus off the address field"
    );
}

#[test]
fn a_press_on_a_system_page_blurs_the_address_field() {
    // The system-page press paths answer the click and return before the
    // toolbar's own handler, so this is the case that regressed.
    let mut browser = Browser::new(LongLoader);
    browser.open_system(SystemPage::Settings);
    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(1024, 768, 1.0));

    browser.ui.focus_address();
    assert!(browser.ui.address_focused(), "the address starts focused");

    browser.on_event(PlatformEvent::PointerMoved { x: 500.0, y: 400.0 });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    assert!(
        !browser.ui.address_focused(),
        "a press on a system page blurs the address field too"
    );
}

#[test]
fn the_interface_and_the_page_both_reach_the_paint_seam() {
    let mut browser = browser();
    type_url(&mut browser, "example.com");

    let mut painter = otlyra_gfx::RecordingPainter::new();
    browser.paint(&mut painter, Viewport::new(800, 600, 2.0));
    let ops = painter.take();

    assert!(
        ops.iter()
            .filter(|op| matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { .. }))
            .count()
            >= 2,
        "the page's text and the interface's own"
    );
}
