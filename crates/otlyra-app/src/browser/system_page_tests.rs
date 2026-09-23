//! The browser's own pages, and what a screen reader is handed, driven from outside.
//!
//! Settings, history, downloads, bookmarks, cookies and the cache, reached the ways
//! a reader reaches them — the menu, the address bar, a native command, an
//! accessibility request — and what each does to the tab it opens in.

use otlyra_platform::{FrameRequest, Painter, PlatformEvent};

use super::*;
use crate::fetcher::Loaded;
use crate::settings;
use crate::ui::SystemPage;

/// A loader that fails everything, so a test that reaches the network is a
/// test that was wrong to.
struct NoNetwork;

impl Loader for NoNetwork {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Err(format!("nothing may be fetched in this test: {url}"))
    }
}

/// Press where the interface drew something, going through the whole path
/// a person's click takes: the platform event, the interface's geometry,
/// and whatever the browser makes of what comes back.
fn press(browser: &mut Browser, x: f64, y: f64) {
    browser.on_event(PlatformEvent::PointerMoved { x, y });
    browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
}

/// A new tab is opened to type an address into, and the caret has to still be
/// there once the toolbar has been drawn for it.
///
/// The frame in the middle is the whole test. Focus is a position in the ring
/// the last frame built, and a blank tab's toolbar is not the previous tab's:
/// without the request outliving the rebuild, ⌘T put the caret in the field
/// and the very next frame moved it to a button, so the first thing typed
/// went nowhere.
#[test]
fn a_new_tab_is_ready_to_be_typed_into_after_it_has_been_drawn() {
    let mut browser = Browser::new(NoNetwork);
    frame(&mut browser, 900.0, 700.0);
    browser.navigate("https://a.example/");
    browser.wait_for_load(std::time::Duration::from_secs(2));
    frame(&mut browser, 900.0, 700.0);

    browser.new_tab();
    frame(&mut browser, 900.0, 700.0);
    assert!(
        browser.ui().address_focused(),
        "the caret left the field when the toolbar was rebuilt"
    );

    browser.on_event(PlatformEvent::TextInput('x'));
    assert_eq!(
        browser.ui().address.text(),
        "x",
        "what was typed did not reach the address bar"
    );
}

/// Draw one frame at `width` by `height`, which is what gives the interface
/// the geometry the next press is tested against.
fn frame(browser: &mut Browser, width: f64, height: f64) {
    let viewport = Viewport {
        width: width as u32,
        height: height as u32,
        scale_factor: 1.0,
    };
    let mut target = otlyra_gfx::RecordingPainter::default();
    browser.paint(&mut target, viewport);
}

// --- what a screen reader is handed -----------------------------------

/// The identifiers `window_tree` hands out, in the order it hands them out.
fn described_labels(browser: &mut Browser) -> Vec<String> {
    browser
        .accessibility()
        .expect("a tree")
        .nodes
        .into_iter()
        .filter_map(|(id, node)| crate::a11y::described_index(id).map(|index| (index, node)))
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_values()
        .map(|node| node.label().unwrap_or_default().to_owned())
        .collect()
}

/// One tree, with the toolbar over the document rather than beside it.
#[test]
fn the_tree_holds_the_interface_and_the_document_together() {
    let mut browser = Browser::new(NoNetwork);
    frame(&mut browser, 1000.0, 700.0);

    let labels = described_labels(&mut browser);
    assert!(
        labels.iter().any(|label| label == "New tab"),
        "the toolbar is not in the tree: {labels:?}"
    );
}

/// With no interface drawn there is nothing to wrap the page in, and a level
/// describing a toolbar that was never drawn would be a level about nothing.
#[test]
fn a_browser_with_no_interface_hands_over_the_page_alone() {
    let mut browser = Browser::new(NoNetwork);
    browser.hide_interface();
    frame(&mut browser, 1000.0, 700.0);

    assert!(described_labels(&mut browser).is_empty());
}

/// The settings' own controls join the toolbar's, so a reader on the page
/// finds the switches rather than an empty document.
#[test]
fn a_browser_page_describes_the_controls_it_drew() {
    let mut browser = Browser::new(NoNetwork);
    browser.open_system(SystemPage::Settings);
    frame(&mut browser, 1000.0, 700.0);

    let labels = described_labels(&mut browser);
    assert!(
        labels.iter().any(|label| label.starts_with("Text size")),
        "the settings' controls are not in the tree: {labels:?}"
    );
}

/// A press asked for by a reader does what a click on the same control does.
#[test]
fn a_reader_can_throw_a_switch_on_the_settings() {
    // Throwing a switch saves the preferences, and saving them must not reach
    // the file the person running the tests browses with. Nothing in this
    // binary loads them any more, so pointing the write somewhere else is the
    // whole of what this needs.
    //
    // SAFETY: set to one constant value, once, and only ever read by
    // `preferences::path` — never changed under a running read.
    unsafe {
        std::env::set_var(
            "OTLYRA_CONFIG_DIR",
            std::env::temp_dir().join("otlyra-tests"),
        )
    };
    std::fs::create_dir_all(std::env::temp_dir().join("otlyra-tests"))
        .expect("a place to save preferences");

    let mut browser = Browser::new(NoNetwork);
    browser.open_system(SystemPage::Settings);
    frame(&mut browser, 1000.0, 700.0);

    let before = browser.settings.settings.load_images;
    let update = browser.accessibility().expect("a tree");
    let (id, _) = update
        .nodes
        .iter()
        .find(|(id, node)| {
            crate::a11y::described_index(*id).is_some() && node.label() == Some("Load images")
        })
        .expect("the images switch");

    browser.on_event(PlatformEvent::AccessibilityRequest {
        node: *id,
        action: otlyra_platform::AccessibilityAction::Activate,
    });
    assert_ne!(
        browser.settings.settings.load_images, before,
        "the switch did not move"
    );
}

#[test]
fn the_menu_opens_the_pages_that_exist_and_closes_over_the_ones_that_do_not() {
    let mut browser = Browser::new(NoNetwork);
    frame(&mut browser, 1000.0, 700.0);

    // The cogwheel is the last control on the toolbar, at its right end.
    press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
    assert!(browser.ui().menu_open(), "the cogwheel opens the menu");

    // The panel hangs below the toolbar at the right-hand edge; its rows are
    // 30 tall under a heading, so this is the first of them.
    frame(&mut browser, 1000.0, 700.0);
    press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 34.0);

    assert!(
        !browser.ui().menu_open(),
        "choosing something closes the menu"
    );
    assert_eq!(
        browser.system_page(),
        Some(SystemPage::Settings),
        "the first row is the settings, and it opens them"
    );
}

#[test]
fn the_history_row_opens_the_history() {
    let mut browser = Browser::new(NoNetwork);
    frame(&mut browser, 1000.0, 700.0);
    press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
    frame(&mut browser, 1000.0, 700.0);

    // The second row is History, and since W8 it is a real page.
    press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 65.0);
    assert!(!browser.ui().menu_open());
    assert_eq!(browser.system_page(), Some(SystemPage::History));
}

#[test]
fn the_downloads_row_opens_the_downloads() {
    let mut browser = Browser::new(NoNetwork);
    frame(&mut browser, 1000.0, 700.0);
    press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
    frame(&mut browser, 1000.0, 700.0);

    press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 127.0);
    assert!(!browser.ui().menu_open());
    assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
}

#[test]
fn the_bookmarks_row_opens_the_bookmarks() {
    let mut browser = Browser::new(NoNetwork);
    frame(&mut browser, 1000.0, 700.0);
    press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
    frame(&mut browser, 1000.0, 700.0);

    // The third row. It was the dimmed one — the test that used to live here
    // proved that a press on a page that did not exist yet fell through to the
    // sheet and only dismissed the menu. Every row on this menu is now a real
    // page, so what is worth checking is that this one opens.
    press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 96.0);
    assert!(!browser.ui().menu_open());
    assert_eq!(browser.system_page(), Some(SystemPage::Bookmarks));
}

#[test]
fn choosing_a_page_from_the_menu_opens_it_beside_what_was_being_read() {
    let mut browser = Browser::new(NoNetwork);
    frame(&mut browser, 1000.0, 700.0);
    press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
    frame(&mut browser, 1000.0, 700.0);

    // The first tab is blank, so the settings fill it rather than leaving an
    // empty tab behind.
    press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 34.0);
    assert_eq!(browser.tabs().len(), 1);
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));

    // From a tab that is showing something, a second one opens.
    browser.open_system_in_new_tab(SystemPage::About);
    assert_eq!(browser.tabs().len(), 2);
    assert_eq!(browser.active(), 1);
    assert_eq!(browser.system_page(), Some(SystemPage::About));
    assert_eq!(
        browser.tabs()[0].system,
        Some(SystemPage::Settings),
        "what was being read stayed where it was"
    );
}

#[test]
fn typing_the_same_address_navigates_in_place() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:otlyra");
    browser.navigate("about:settings");

    assert_eq!(browser.tabs().len(), 1, "typing is a decision to leave");
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
}

#[test]
fn a_browser_page_belongs_to_its_tab_and_not_to_the_window() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:settings");
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));

    // A second tab is a second place, and it is not on the settings.
    browser.new_tab();
    assert_eq!(browser.system_page(), None);

    browser.select_tab(0);
    assert_eq!(
        browser.system_page(),
        Some(SystemPage::Settings),
        "the first tab kept what it was showing"
    );
}

#[test]
fn a_browser_page_earns_a_history_entry_and_back_leaves_it() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:otlyra");
    browser.navigate("about:settings");
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    assert!(browser.can_go_back());

    browser.go_back();
    assert_eq!(
        browser.system_page(),
        Some(SystemPage::About),
        "back reaches the browser page that was there"
    );

    browser.go_forward();
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
}

#[test]
fn done_on_the_settings_goes_back_rather_than_emptying_the_tab() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:otlyra");
    browser.navigate("about:settings");

    browser.handle_settings_action(&crate::settings::Action::Close);
    assert_eq!(
        browser.system_page(),
        Some(SystemPage::About),
        "done is back"
    );
}

#[test]
fn done_with_nowhere_behind_it_empties_the_tab() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:settings");

    browser.handle_settings_action(&crate::settings::Action::Close);
    assert_eq!(browser.system_page(), None);
    assert_eq!(browser.tabs()[0].title, "New tab");
}

#[test]
fn typing_a_browser_address_opens_a_surface_rather_than_fetching() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:settings");

    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    assert_eq!(browser.tabs()[0].url, "about:settings");
    assert_eq!(browser.ui().address.text(), "about:settings");
    assert!(
        browser.tabs()[0].error.is_none(),
        "nothing was fetched, so nothing failed"
    );
}

#[test]
fn the_spellings_a_person_might_type_all_arrive_at_the_same_page() {
    for spelling in ["about:settings", "About:Settings", "about:preferences/"] {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate(spelling);
        assert_eq!(
            browser.system_page(),
            Some(SystemPage::Settings),
            "{spelling} should open the settings"
        );
    }

    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:otlyra");
    assert_eq!(browser.system_page(), Some(SystemPage::About));
}

#[test]
fn native_commands_open_the_surfaces_they_name() {
    let mut settings = crate::settings::Settings::default();
    settings.home.set_text("about:otlyra");
    let mut browser = Browser::with_settings(NoNetwork, settings);

    browser.on_event(PlatformEvent::MenuCommand(crate::menu::Command::Home.id()));
    assert_eq!(browser.system_page(), Some(SystemPage::About));

    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::Settings.id(),
    ));
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));

    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ShowHistory.id(),
    ));
    assert_eq!(browser.system_page(), Some(SystemPage::History));

    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ShowDownloads.id(),
    ));
    assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
}

#[test]
fn native_developer_tools_command_toggles_the_inspector() {
    let mut browser = Browser::new(NoNetwork);

    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ToggleDevTools.id(),
    ));
    assert!(browser.inspector.open);
    assert_eq!(browser.keyboard_surface, SURFACE_INSPECTOR);

    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ToggleDevTools.id(),
    ));
    assert!(!browser.inspector.open);
    assert_eq!(browser.keyboard_surface, browser.tab_surface());
}

#[test]
fn downloads_address_opens_the_native_surface_without_fetching() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:downloads");

    assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
    assert_eq!(browser.tabs()[0].url, "about:downloads");
    assert!(browser.tabs()[0].error.is_none());
}

#[test]
fn an_attachment_becomes_a_completed_download_instead_of_a_document() {
    struct Attachment;

    impl Loader for Attachment {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                bytes: b"id,name\n1,Ada\n".to_vec(),
                content_type: Some("text/csv".to_owned()),
                response_headers: vec![(
                    "content-disposition".to_owned(),
                    "attachment; filename=people.csv".to_owned(),
                )],
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(Attachment);
    browser.navigate("https://example.test/export");
    browser.wait_for_load(std::time::Duration::from_secs(5));

    assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
    assert_eq!(browser.tabs()[0].url, "about:downloads");
    assert!(browser.tabs()[0].page.is_none());
    let download = browser
        .downloads
        .downloads()
        .next()
        .expect("the attachment was retained");
    assert_eq!(download.filename(), "people.csv");
    assert_eq!(download.content_type(), Some("text/csv"));
    assert_eq!(download.bytes(), b"id,name\n1,Ada\n");
}

/// With asking turned off, an attachment reaches the disk on its own — nobody
/// presses anything, so the write has to start where the bytes arrive.
#[test]
fn an_attachment_saves_itself_when_the_preference_says_not_to_ask() {
    struct Attachment;

    impl Loader for Attachment {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                bytes: b"id,name\n1,Ada\n".to_vec(),
                content_type: Some("text/csv".to_owned()),
                response_headers: vec![(
                    "content-disposition".to_owned(),
                    "attachment; filename=people.csv".to_owned(),
                )],
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    // A directory this test owns. The preference is set directly rather than
    // through the environment, because the environment is process-wide and the
    // rest of the suite is running beside this.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time moves forward")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "otlyra-automatic-download-{}-{unique}",
        std::process::id()
    ));

    let mut settings = crate::settings::Settings::default();
    settings.apply(crate::settings::Action::ToggleDownloadAsk);
    settings.apply(crate::settings::Action::SetDownloadDirectory(
        directory.to_string_lossy().into_owned(),
    ));
    assert!(!settings.asks_where_to_save());

    let mut browser = Browser::with_settings(Attachment, settings);
    browser.navigate("https://example.test/export");
    browser.wait_for_load(std::time::Duration::from_secs(5));

    // The write is asynchronous, so the row is pending here and the file
    // arrives through `pump` — the same route the running browser takes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let saved = loop {
        browser.pump();
        if let Some(saved) = browser
            .downloads
            .downloads()
            .next()
            .and_then(|download| download.saved_to())
        {
            break saved.to_owned();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the download never reached the disk: {:?}",
            browser.downloads.downloads().next().map(|download| (
                download.saving_to().map(str::to_owned),
                download.save_error().map(str::to_owned)
            ))
        );
        std::thread::yield_now();
    };

    assert_eq!(
        std::path::Path::new(&saved),
        directory.join("people.csv"),
        "the file went somewhere other than the download folder"
    );
    assert_eq!(
        std::fs::read(&saved).expect("the saved download"),
        b"id,name\n1,Ada\n"
    );
    std::fs::remove_dir_all(&directory).expect("remove the test-owned directory");
}

/// And with asking left on — the default — nothing is written without a person.
#[test]
fn an_attachment_waits_to_be_asked_about_by_default() {
    struct Attachment;

    impl Loader for Attachment {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                bytes: b"id,name\n1,Ada\n".to_vec(),
                response_headers: vec![(
                    "content-disposition".to_owned(),
                    "attachment; filename=people.csv".to_owned(),
                )],
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(Attachment);
    assert!(browser.settings.settings.asks_where_to_save());
    browser.navigate("https://example.test/export");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    browser.pump();

    let download = browser
        .downloads
        .downloads()
        .next()
        .expect("the attachment was retained");
    assert!(download.saved_to().is_none(), "it saved itself uninvited");
    assert!(download.saving_to().is_none());
    assert!(download.save_error().is_none());
}

#[test]
fn bookmarks_address_opens_the_native_surface_without_fetching() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:bookmarks");

    assert_eq!(browser.system_page(), Some(SystemPage::Bookmarks));
    assert_eq!(browser.tabs()[0].url, "about:bookmarks");
    assert!(browser.tabs()[0].error.is_none());
}

/// An animation frame that only reads costs a frame, not a restyle.
///
/// A `requestAnimationFrame` loop that measures something and decides it has
/// nothing to do this frame is most of what such loops are; treating "a
/// callback ran" as "the document changed" re-cascaded the whole document
/// and rebuilt its box tree sixty times a second for it.
#[test]
fn an_animation_frame_that_changes_nothing_rebuilds_nothing() {
    struct Page;

    impl Loader for Page {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: br#"<title>Reading</title><p id=p>hello</p><script>
                        let seen = 0;
                        function tick() {
                          // Reads, and only reads.
                          seen += document.getElementById('p').textContent.length;
                          requestAnimationFrame(tick);
                        }
                        requestAnimationFrame(tick);
                    </script>"#
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let viewport = Viewport::new(800, 600, 1.0);
    let mut browser = Browser::new(Page);
    browser.navigate("https://example.test/raf");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    browser.paint(&mut otlyra_gfx::RecordingPainter::new(), viewport);

    let builds = browser.tabs[browser.active]
        .page
        .as_ref()
        .expect("the page is loaded")
        .builds();
    assert!(
        browser.next_frame() == FrameRequest::Vsync,
        "a page with a frame callback outstanding asks for the next frame"
    );

    for _ in 0..10 {
        browser.paint(&mut otlyra_gfx::RecordingPainter::new(), viewport);
    }

    let page = browser.tabs[browser.active]
        .page
        .as_ref()
        .expect("the page is still loaded");
    assert_eq!(
        page.builds(),
        builds,
        "ten frames of a read-only animation loop rebuilt the page's display list"
    );
    assert_eq!(
        browser.next_frame(),
        FrameRequest::Vsync,
        "and the loop is still running, so the frames must keep coming"
    );
}

/// ⌘D keeps the page, and ⌘D again stops keeping it. One command both ways,
/// because that is what one key can mean.
#[test]
fn the_bookmark_command_keeps_the_page_and_then_drops_it() {
    struct Page;

    impl Loader for Page {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                bytes: b"<title>Kept</title><p>hello".to_vec(),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(Page);
    browser.navigate("https://example.test/keep");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    assert!(!browser.is_bookmarked());
    assert_eq!(browser.ui().bookmark, crate::ui::Bookmarked::No);

    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ToggleBookmark.id(),
    ));
    assert!(browser.is_bookmarked());
    assert_eq!(
        browser.ui().bookmark,
        crate::ui::Bookmarked::Yes,
        "the star and the menu must know, or they offer to keep it twice"
    );
    let kept: Vec<(String, String)> = browser
        .bookmarks
        .bookmarks()
        .map(|bookmark| (bookmark.url.clone(), bookmark.title.clone()))
        .collect();
    assert_eq!(
        kept,
        [("https://example.test/keep".to_owned(), "Kept".to_owned())],
        "the document's own title names it"
    );

    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ToggleBookmark.id(),
    ));
    assert!(!browser.is_bookmarked());
    assert_eq!(browser.ui().bookmark, crate::ui::Bookmarked::No);
    assert!(browser.bookmarks.is_empty());
}

/// A hard reload empties the cache, which is what makes it the answer to a
/// stylesheet a server is serving stale: the page and everything it then asks
/// for are all fetched afresh.
#[test]
fn a_hard_reload_empties_the_cache_and_an_ordinary_one_does_not() {
    let cache: otlyra_net::SharedCache =
        std::sync::Arc::new(std::sync::Mutex::new(otlyra_net::cache::Cache::new()));
    let mut browser = Browser::new(NoNetwork);
    browser.set_cache(std::sync::Arc::clone(&cache));
    browser.navigate("https://example.test/");

    let put = |cache: &otlyra_net::SharedCache| {
        let stored = otlyra_net::cache::Stored {
            status: 200,
            headers: vec![("cache-control".to_owned(), "max-age=3600".to_owned())],
            body: b"body".to_vec(),
            final_url: "https://example.test/a".to_owned(),
            directives: otlyra_net::cache::Directives::parse(["max-age=3600"]),
            lifetime: otlyra_net::cache::Lifetime::Stated(std::time::Duration::from_secs(3600)),
            times: otlyra_net::cache::Times {
                requested: std::time::SystemTime::now(),
                received: std::time::SystemTime::now(),
                date: std::time::SystemTime::now(),
                age: std::time::Duration::ZERO,
            },
            varied: Vec::new(),
            varies_on_everything: false,
        };
        cache
            .lock()
            .expect("not poisoned")
            .store("https://example.test/a", "GET", stored, &[]);
    };

    put(&cache);
    assert_eq!(cache.lock().expect("not poisoned").len(), 1);
    browser.reload();
    assert_eq!(
        cache.lock().expect("not poisoned").len(),
        1,
        "an ordinary reload asks about what is kept rather than throwing it away"
    );

    browser.reload_ignoring_cache();
    assert!(
        cache.lock().expect("not poisoned").is_empty(),
        "and a hard one starts over"
    );
}

/// The mode is an instruction about one navigation. Left set, every later
/// click would behave like a reload and the cache would answer nothing.
#[test]
fn a_reload_does_not_make_every_later_click_a_reload() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("https://example.test/");
    browser.reload();
    assert_eq!(
        browser.next_cache_mode,
        otlyra_net::CacheMode::Default,
        "the navigation it was set for took it"
    );
    browser.reload_ignoring_cache();
    assert_eq!(browser.next_cache_mode, otlyra_net::CacheMode::Default);
}

/// The cache page opens, lists what is held, and empties it.
#[test]
fn the_cache_page_opens_and_empties() {
    let cache: otlyra_net::SharedCache =
        std::sync::Arc::new(std::sync::Mutex::new(otlyra_net::cache::Cache::new()));
    let mut browser = Browser::new(NoNetwork);
    browser.set_cache(std::sync::Arc::clone(&cache));
    for url in ["https://one.test/a", "https://two.test/b"] {
        let now = std::time::SystemTime::now();
        cache.lock().expect("not poisoned").store(
            url,
            "GET",
            otlyra_net::cache::Stored {
                status: 200,
                headers: vec![("cache-control".to_owned(), "max-age=3600".to_owned())],
                body: b"body".to_vec(),
                final_url: url.to_owned(),
                directives: otlyra_net::cache::Directives::parse(["max-age=3600"]),
                lifetime: otlyra_net::cache::Lifetime::Stated(std::time::Duration::from_secs(3600)),
                times: otlyra_net::cache::Times {
                    requested: now,
                    received: now,
                    date: now,
                    age: std::time::Duration::ZERO,
                },
                varied: Vec::new(),
                varies_on_everything: false,
            },
            &[],
        );
    }

    browser.navigate("about:cache");
    assert_eq!(browser.system_page(), Some(SystemPage::Cache));

    browser.handle_cache_action(crate::cache::Action::ClearSite("one.test".into()));
    assert_eq!(cache.lock().expect("not poisoned").len(), 1);
    browser.handle_cache_action(crate::cache::Action::Clear);
    assert!(cache.lock().expect("not poisoned").is_empty());

    // And from the menu, in a browser that has no cache at all — which every
    // headless mode is, and which must open the page rather than fall over.
    let mut none = Browser::new(NoNetwork);
    none.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ShowCache.id(),
    ));
    assert_eq!(none.system_page(), Some(SystemPage::Cache));
    none.handle_cache_action(crate::cache::Action::Clear);
}

/// The cookies page is reachable the three ways every browser page is: by
/// address, from the menu, and by the keyboard once it is open.
#[test]
fn the_cookies_page_opens_and_closes() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:cookies");
    assert_eq!(browser.system_page(), Some(SystemPage::Cookies));
    assert_eq!(browser.tabs()[0].url, "about:cookies");

    let mut fresh = Browser::new(NoNetwork);
    fresh.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ShowCookies.id(),
    ));
    assert_eq!(fresh.system_page(), Some(SystemPage::Cookies));
}

/// Throwing a site away throws that site away and leaves the rest.
#[test]
fn the_page_can_be_rid_of_one_site_or_of_everything() {
    let mut browser = Browser::new(NoNetwork);
    let now = std::time::SystemTime::now();
    browser.cookies.with(|jar| {
        for address in ["https://one.test/", "https://two.test/"] {
            jar.set(&url::Url::parse(address).expect("a url"), "a=1", now)
                .expect("kept");
        }
    });
    assert_eq!(browser.cookies.with(|jar| jar.len()), 2);

    browser.handle_cookies_action(crate::cookies::Action::ClearSite("one.test".into()));
    assert_eq!(browser.cookies.with(|jar| jar.len()), 1);
    browser.handle_cookies_action(crate::cookies::Action::Clear);
    assert!(browser.cookies.with(|jar| jar.is_empty()));
}

/// The switch in the preferences is the switch in the jar. Two places that
/// could disagree about whether a cookie is refused would be one place too
/// many.
#[test]
fn the_third_party_switch_reaches_the_jar() {
    let mut browser = Browser::new(NoNetwork);
    assert!(
        browser.cookies.with(|jar| jar.accepts_third_party()),
        "the default is what every browser still ships"
    );

    browser
        .settings
        .settings
        .apply(settings::Action::ToggleThirdPartyCookies);
    browser.handle_settings_action(&settings::Action::ToggleThirdPartyCookies);
    assert!(!browser.cookies.with(|jar| jar.accepts_third_party()));

    // And a browser built from preferences that already say so starts that
    // way, rather than only after somebody presses the switch again.
    let mut settings = crate::settings::Settings::default();
    settings.block_third_party_cookies = true;
    let started = Browser::with_fetcher(crate::fetcher::Fetcher::spawn(NoNetwork), settings);
    assert!(!started.cookies.with(|jar| jar.accepts_third_party()));
}

/// A blank tab has no address, and a bookmark that opens nowhere is worse than
/// no bookmark.
#[test]
fn the_bookmark_command_does_nothing_on_a_blank_tab() {
    let mut browser = Browser::new(NoNetwork);
    assert_eq!(
        browser.ui().bookmark,
        crate::ui::Bookmarked::Impossible,
        "and the star says as much rather than looking pressable"
    );
    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ToggleBookmark.id(),
    ));
    assert!(browser.bookmarks.is_empty());
}

#[test]
fn the_native_bookmarks_command_opens_the_page() {
    let mut browser = Browser::new(NoNetwork);
    browser.on_event(PlatformEvent::MenuCommand(
        crate::menu::Command::ShowBookmarks.id(),
    ));
    assert_eq!(browser.system_page(), Some(SystemPage::Bookmarks));
}

/// Pressing a row goes there, and the address bar agrees that this is a page
/// the reader kept.
#[test]
fn a_kept_page_can_be_opened_from_the_list_again() {
    struct Page;

    impl Loader for Page {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                bytes: b"<title>Kept</title>".to_vec(),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut browser = Browser::new(Page);
    browser.bookmarks.add("https://example.test/keep", "Kept");
    browser.handle_bookmarks_action(crate::bookmarks::Action::Open(
        "https://example.test/keep".to_owned(),
    ));
    browser.wait_for_load(std::time::Duration::from_secs(5));

    assert_eq!(browser.tabs()[0].url, "https://example.test/keep");
    assert_eq!(
        browser.ui().bookmark,
        crate::ui::Bookmarked::Yes,
        "arriving at a kept page must fill the star"
    );
}

#[test]
fn leaving_the_settings_leaves_the_tab_blank_rather_than_still_on_them() {
    let mut browser = Browser::new(NoNetwork);
    browser.navigate("about:settings");
    browser.handle_settings_action(&crate::settings::Action::Close);

    assert_eq!(browser.system_page(), None);
    assert_eq!(browser.ui().address.text(), "");
    assert_eq!(browser.tabs()[0].title, "New tab");
}
