//! What the *window* shows after real input.
//!
//! Every other test in this crate renders through `paint` — one whole surface,
//! offscreen, with no compositor between the display list and the assertion. A
//! regression that lives in the retained compositor, in a layer's epoch, or in a
//! model change that never reaches a layer at all is invisible to those tests and
//! plainly visible to a person using the browser.
//!
//! So this drives a window through the automation protocol — pointer actions in,
//! composited pixels out — and asserts on the pixels. The window is the same
//! `compose` → damage → retained-surface path the live window runs; only the
//! swapchain is missing, and a swapchain cannot change what was drawn.

use serde_json::{Value, json};

use otlyra_app::bidi::{Command, Session};
use otlyra_app::browser::Browser;
use otlyra_app::fetcher::{Loaded, Loader};
use otlyra_app::ui::UI_HEIGHT;
use otlyra_platform::{Damage, FramePump, Key, Modifiers, PlatformEvent, Viewport};

/// Where the field is on the page, in CSS pixels, and how big it is. Stated here
/// because the assertions look at exactly these pixels.
const FIELD: (u32, u32, u32, u32) = (40, 40, 240, 32);

/// A page with one text field near the top and a paragraph far below it, so a
/// press "away from the field" is a press on plain page and not on some other
/// control.
const PAGE: &str = r#"<!doctype html><meta charset=utf-8>
<body style="margin:0;background:#ffffff">
<input id=field value="hello" style="position:absolute;left:40px;top:40px;width:240px;height:32px;font-size:16px">
<p id=away style="position:absolute;left:40px;top:360px;margin:0">a place to click that is not the field</p>
"#;

struct Pages;

impl Loader for Pages {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            content_type: Some("text/html".to_owned()),
            bytes: PAGE.as_bytes().to_vec(),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

struct FlowPage;

impl Loader for FlowPage {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        let paragraphs = "<p>ordinary text around the field</p>".repeat(40);
        Ok(Loaded {
            content_type: Some("text/html".to_owned()),
            bytes: format!(
                "<!doctype html><meta charset=utf-8><body style=\"margin:0\">\
                 <input id=field value=\"hello\" style=\"position:absolute;left:40px;top:40px;\
                 width:240px;height:32px;font-size:16px\">\
                 <div style=\"margin-top:100px\">{paragraphs}</div>"
            )
            .into_bytes(),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

struct EmptyFlowPage;

impl Loader for EmptyFlowPage {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        let paragraphs = "<p>ordinary text around the field</p>".repeat(40);
        Ok(Loaded {
            content_type: Some("text/html".to_owned()),
            bytes: format!(
                "<!doctype html><meta charset=utf-8><body style=\"margin:0\">\
                 <input id=field style=\"position:absolute;left:40px;top:40px;\
                 width:320px;height:32px;font-size:16px\">\
                 <div style=\"margin-top:100px\">{paragraphs}</div>"
            )
            .into_bytes(),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

fn ask(session: &mut Session, method: &str, params: Value) -> Value {
    session
        .dispatch(&Command {
            id: 1,
            method: method.to_owned(),
            params,
        })
        .unwrap_or_else(|error| panic!("{method}: {}: {}", error.code, error.message))
}

/// A window session that has already loaded the page.
fn driven() -> Session {
    let mut session = Session::windowed(Browser::new(Pages), (900, 640));
    ask(&mut session, "session.new", json!({}));
    ask(
        &mut session,
        "browsingContext.navigate",
        json!({ "url": "https://form.example/" }),
    );
    session
}

/// One press and release, wherever `origin` aims.
fn click(session: &mut Session, origin: Value) {
    ask(
        session,
        "input.performActions",
        json!({
            "actions": [{
                "type": "pointer",
                "id": "mouse",
                "actions": [
                    origin,
                    { "type": "pointerDown", "button": 0 },
                    { "type": "pointerUp", "button": 0 },
                ],
            }],
        }),
    );
}

/// A press in the middle of the element a selector finds.
fn click_element(session: &mut Session, selector: &str) {
    let found = ask(
        session,
        "browsingContext.locateNodes",
        json!({ "locator": { "type": "css", "value": selector } }),
    );
    let shared = found["nodes"][0]["sharedId"]
        .as_str()
        .unwrap_or_else(|| panic!("{selector} matched nothing: {found}"))
        .to_owned();
    click(
        session,
        json!({
            "type": "pointerMove",
            "x": 0,
            "y": 0,
            "origin": { "type": "element", "element": { "sharedId": shared } },
        }),
    );
}

/// A press at a point in the window, chrome included.
fn click_at(session: &mut Session, x: f64, y: f64) {
    click(session, json!({ "type": "pointerMove", "x": x, "y": y }));
}

/// The window as it is now: its pixels, and what the last frame redrew.
fn window(session: &mut Session) -> (Vec<u8>, u32, Value) {
    let captured = ask(session, "otlyra:captureWindow", json!({}));
    let png = base64_decode(captured["data"].as_str().expect("a PNG in base64"));
    let (width, pixels) = decode(&png);
    (pixels, width, captured["damage"].clone())
}

/// The bytes of one rectangle of a captured window, row by row.
fn region(pixels: &[u8], width: u32, rect: (u32, u32, u32, u32)) -> Vec<u8> {
    let (x, y, w, h) = rect;
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in y..y + h {
        let start = ((row * width + x) * 4) as usize;
        out.extend_from_slice(&pixels[start..start + (w * 4) as usize]);
    }
    out
}

/// Where the field's pixels are in the window: the page sits under the chrome.
fn field_in_window() -> (u32, u32, u32, u32) {
    let (x, y, w, h) = FIELD;
    (x, y + UI_HEIGHT as u32, w, h)
}

/// A captured PNG as its width and its RGBA bytes.
fn decode(png: &[u8]) -> (u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().expect("valid PNG header");
    let mut buffer = vec![0; reader.output_buffer_size().expect("known buffer size")];
    let info = reader.next_frame(&mut buffer).expect("valid PNG body");
    buffer.truncate(info.buffer_size());
    (info.width, buffer)
}

fn base64_decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let value = ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .unwrap_or_else(|| panic!("{} is not base64", byte as char)) as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}

#[test]
fn a_press_away_from_a_field_clears_what_the_field_was_showing() {
    let mut session = driven();

    // Before anything is clicked, so the field's own resting appearance is what
    // the last capture is compared against rather than a guess about it.
    let (resting, width, _) = window(&mut session);

    click_element(&mut session, "#field");
    let (focused, _, focused_damage) = window(&mut session);
    assert_ne!(
        region(&resting, width, field_in_window()),
        region(&focused, width, field_in_window()),
        "a press in the field changed nothing on screen",
    );
    assert!(
        !focused_damage.is_null(),
        "a press in the field composited no damage",
    );

    // Now away from it, onto plain page. What a person sees has to go back to
    // what it was: no caret, no focus ring.
    click_at(&mut session, 600.0, UI_HEIGHT + 500.0);
    let (blurred, _, blurred_damage) = window(&mut session);
    assert!(
        !blurred_damage.is_null(),
        "the press away from the field composited no damage: the window still \
         shows the caret and the ring",
    );
    assert_eq!(
        region(&resting, width, field_in_window()),
        region(&blurred, width, field_in_window()),
        "the field still looks focused after a press somewhere else",
    );
    assert_eq!(
        resting, blurred,
        "the click painted a caret or another editing artifact into plain page text",
    );
}

#[test]
fn a_press_on_the_toolbar_leaves_the_page_alone() {
    let mut session = driven();
    let (before, width, _) = window(&mut session);

    // The reload button's row, which is chrome and nothing else. The damage the
    // compositor reports must stay inside the chrome band: a chrome-only press
    // that re-rasterizes the page is the cost this compositor exists to avoid.
    click_at(&mut session, 300.0, UI_HEIGHT - 20.0);
    let (after, _, damage) = window(&mut session);

    if let Some(rect) = damage.as_object() {
        let bottom =
            rect["y"].as_u64().expect("a top") + rect["height"].as_u64().expect("a height");
        assert!(
            bottom <= UI_HEIGHT as u64,
            "a press on the toolbar damaged the page: {damage}",
        );
    }
    let page = (0, UI_HEIGHT as u32, width, 200);
    assert_eq!(
        region(&before, width, page),
        region(&after, width, page),
        "a press on the toolbar changed the page's pixels",
    );
}

#[test]
fn the_browser_menu_is_visible_below_the_composited_toolbar() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut browser = Browser::new(Pages);
    browser.navigate("https://form.example/");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    browser.prepare_frame(viewport, std::time::Duration::from_secs(5));

    let mut pump = FramePump::new(viewport);
    pump.open(&mut browser).expect("the closed-menu frame");
    let before = pump.png().expect("the closed window");

    pump.event(
        &mut browser,
        PlatformEvent::PointerMoved {
            x: 900.0 - 22.0,
            y: UI_HEIGHT - 21.0,
        },
    );
    pump.event(&mut browser, PlatformEvent::PointerPressed { clicks: 1 });
    pump.event(&mut browser, PlatformEvent::PointerReleased);
    pump.frame(&mut browser).expect("the open-menu frame");
    let after = pump.png().expect("the open window");

    let (width, before) = decode(&before);
    let (_, after) = decode(&after);
    let popup = (640, UI_HEIGHT as u32, 260, 230);
    assert_ne!(
        region(&before, width, popup),
        region(&after, width, popup),
        "the menu state changed but the compositor clipped its popup"
    );

    // The retained window and whole-surface path must still draw the same menu.
    let painted =
        otlyra_platform::render_offscreen(&mut browser, viewport).expect("the whole-surface menu");
    let (_, painted) = decode(&painted);
    assert_eq!(
        after, painted,
        "the retained compositor and whole-surface path disagree on the open menu"
    );
}

#[test]
fn typing_in_a_page_field_damages_only_the_field() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut browser = Browser::new(FlowPage);
    browser.navigate("https://form.example/");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    browser.prepare_frame(viewport, std::time::Duration::from_secs(5));

    let mut pump = FramePump::new(viewport);
    pump.open(&mut browser).expect("a first frame");
    let field = field_in_window();
    let x = f64::from(field.0 + field.2 / 2);
    let y = f64::from(field.1 + field.3 / 2);
    pump.event(&mut browser, PlatformEvent::PointerMoved { x, y });
    pump.event(&mut browser, PlatformEvent::PointerPressed { clicks: 1 });
    pump.event(&mut browser, PlatformEvent::PointerReleased);
    pump.frame(&mut browser).expect("the focused frame");

    pump.event(&mut browser, PlatformEvent::TextInput('x'));
    pump.frame(&mut browser).expect("the typed frame");

    let Damage::Region(dirty) = pump.damage() else {
        panic!("typing damaged more than one region: {:?}", pump.damage());
    };
    // CSS width/height name the content box; the user-agent border and padding
    // add a few pixels around the constants above.
    let allowance = 8;
    assert!(
        dirty.x >= field.0.saturating_sub(allowance)
            && dirty.y >= field.1.saturating_sub(allowance)
            && dirty.x + dirty.width <= field.0 + field.2 + allowance
            && dirty.y + dirty.height <= field.1 + field.3 + allowance,
        "typing damaged pixels outside the field: {dirty:?}, field {field:?}",
    );
}

#[test]
fn continued_typing_at_hidpi_damages_only_the_field() {
    let viewport = Viewport::new(2048, 1536, 2.0);
    let mut browser = Browser::new(EmptyFlowPage);
    browser.navigate("https://form.example/");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    browser.prepare_frame(viewport, std::time::Duration::from_secs(5));

    let mut pump = FramePump::new(viewport);
    pump.open(&mut browser).expect("a first frame");
    pump.event(
        &mut browser,
        PlatformEvent::PointerMoved {
            x: 200.0,
            y: UI_HEIGHT + 56.0,
        },
    );
    pump.event(&mut browser, PlatformEvent::PointerPressed { clicks: 1 });
    pump.event(&mut browser, PlatformEvent::PointerReleased);
    pump.frame(&mut browser).expect("the focused frame");

    // The transition from no generated text box to one needs the full builder.
    pump.event(&mut browser, PlatformEvent::TextInput('a'));
    pump.frame(&mut browser).expect("the first typed frame");

    let field = (40_u32, UI_HEIGHT as u32 + 40, 320_u32, 32_u32);
    let allowance = 8;
    for (index, character) in "the quick brown fox jumps over the lazy dog"
        .chars()
        .enumerate()
    {
        pump.event(
            &mut browser,
            PlatformEvent::KeyPressed {
                key: Key::Character(character),
                modifiers: Modifiers::default(),
            },
        );
        pump.event(&mut browser, PlatformEvent::TextInput(character));
        let _ = otlyra_platform::Painter::compose(&mut browser, viewport);
        pump.frame(&mut browser).expect("a continued typed frame");
        let Damage::Region(dirty) = pump.damage() else {
            panic!(
                "character {index} ({character:?}) damaged more than one region: {:?}",
                pump.damage()
            );
        };
        assert!(
            dirty.x >= (field.0.saturating_sub(allowance)) * 2
                && dirty.y >= (field.1.saturating_sub(allowance)) * 2
                && dirty.x + dirty.width <= (field.0 + field.2 + allowance) * 2
                && dirty.y + dirty.height <= (field.1 + field.3 + allowance) * 2,
            "character {index} ({character:?}) damaged pixels outside the field: \
             {dirty:?}, field {field:?}",
        );
    }
}

#[test]
fn text_input_goes_only_to_the_surface_last_pressed() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut browser = Browser::new(FlowPage);
    browser.navigate("https://form.example/");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    browser.prepare_frame(viewport, std::time::Duration::from_secs(5));

    let mut pump = FramePump::new(viewport);
    pump.open(&mut browser).expect("a first frame");

    let inspector_modifiers = Modifiers {
        alt: true,
        command: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        ..Modifiers::default()
    };
    pump.event(
        &mut browser,
        PlatformEvent::KeyPressed {
            key: Key::Character('i'),
            modifiers: inspector_modifiers,
        },
    );
    pump.frame(&mut browser).expect("the inspector frame");

    let accelerator = Modifiers {
        command: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        ..Modifiers::default()
    };
    pump.event(
        &mut browser,
        PlatformEvent::KeyPressed {
            key: Key::Character('f'),
            modifiers: accelerator,
        },
    );
    pump.event(&mut browser, PlatformEvent::TextInput('x'));
    pump.frame(&mut browser)
        .expect("the inspector search frame");
    assert_eq!(browser.inspector_mut().search.text(), "x");

    // The inspector deliberately keeps its search contents after focus leaves.
    // That stale field must not get a second chance when the page owns input.
    pump.event(
        &mut browser,
        PlatformEvent::PointerMoved {
            x: 160.0,
            y: UI_HEIGHT + 56.0,
        },
    );
    pump.event(&mut browser, PlatformEvent::PointerPressed { clicks: 1 });
    pump.event(&mut browser, PlatformEvent::PointerReleased);
    pump.event(&mut browser, PlatformEvent::TextInput('y'));
    pump.frame(&mut browser).expect("the page field frame");

    assert_eq!(
        browser.inspector_mut().search.text(),
        "x",
        "a background inspector field consumed page input"
    );
    assert!(
        browser
            .active_page()
            .and_then(|page| page.focused_value())
            .is_some_and(|value| value.contains('y')),
        "the active page field did not receive the input"
    );
}

/// The window and one whole-surface paint of the same state, as PNG bytes.
///
/// The two paths that draw this browser: the compositor assembles retained
/// layers into a surface, and `paint` draws everything into a fresh one. They
/// share their region builders, so they are meant to be the same picture — and
/// the moment they are not, every golden in this crate is testing something
/// nobody is looking at.
fn both_paths(browser: &mut Browser, viewport: otlyra_platform::Viewport) -> (Vec<u8>, Vec<u8>) {
    let mut pump = otlyra_platform::FramePump::new(viewport);
    pump.open(browser).expect("a composited frame");
    let composed = pump.png().expect("the window as a PNG");
    let painted = otlyra_platform::render_offscreen(browser, viewport).expect("a painted frame");
    (composed, painted)
}

#[test]
fn the_composited_window_is_what_a_whole_surface_paint_would_have_drawn() {
    let viewport = otlyra_platform::Viewport::new(900, 640, 1.0);
    let mut browser = Browser::new(Pages);
    browser.navigate("https://form.example/");
    browser.wait_for_load(std::time::Duration::from_secs(5));
    browser.prepare_frame(viewport, std::time::Duration::from_secs(5));

    // With the inspector open and an element chosen, so the overlay and the
    // panel — the two layers with the most freedom to draw outside their own
    // rectangle — are both in the frame.
    browser.inspect_at(160.0, UI_HEIGHT + 56.0);
    browser.prepare_frame(viewport, std::time::Duration::from_secs(5));

    let (composed, painted) = both_paths(&mut browser, viewport);
    let (width, composed) = decode(&composed);
    let (painted_width, painted) = decode(&painted);

    assert_eq!(width, painted_width);
    let differing = composed
        .chunks(4)
        .zip(painted.chunks(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} pixels differ between the composited window and a whole-surface paint",
    );
}
