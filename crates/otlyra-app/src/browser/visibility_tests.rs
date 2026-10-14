//! Visibility through real tab commands, parsing and JavaScript callbacks.

use std::time::Duration;

use otlyra_platform::{FrameRequest, Painter, Viewport};

use super::*;
use crate::fetcher::{Loaded, Loader};

const URL: &str = "https://visibility.test/";
pub(super) const HTML: &str = r#"<!doctype html><title>Visibility</title><p id=state></p><script>
  const state = document.getElementById('state');
  state.setAttribute('initial', document.visibilityState);
  let frames = 0;
  let changes = 0;
  state.setAttribute('frames', '0');
  state.setAttribute('changes', '0');
  document.addEventListener('visibilitychange', () => {
    state.setAttribute('changes', String(++changes));
    state.setAttribute('visibility', document.visibilityState);
    if (document.hidden) setTimeout(() => state.setAttribute('timer', String(document.hidden)), 0);
  });
  function tick() {
    state.setAttribute('frames', String(++frames));
    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);
</script>"#;

struct Pages;
impl Loader for Pages {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(response(url, HTML))
    }
}

fn response(url: &str, html: &str) -> Loaded {
    Loaded {
        bytes: html.as_bytes().to_vec(),
        charset: Some("utf-8".into()),
        content_type: Some("text/html".into()),
        final_url: url.into(),
        ..Default::default()
    }
}

pub(super) fn attr(browser: &Browser, tab: TabId, name: &str) -> String {
    let document = browser.world.page(tab).unwrap().document();
    let mut pending = vec![document.root()];
    while let Some(node) = pending.pop() {
        if document.attr(node, "id") == Some("state") {
            return document.attr(node, name).unwrap_or("").to_owned();
        }
        pending.extend(document.children(node));
    }
    panic!("visibility fixture has no state element");
}

// Match the native content owner's stack; Otter's debug bootstrap is deep.
fn on_content_stack(run: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name("visibility-test".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn background_animations_pause_timers_continue_and_closing_the_active_tab_resumes() {
    on_content_stack(|| {
        let mut browser = Browser::new(Pages);
        browser.navigate(URL);
        browser.wait_for_load(Duration::from_secs(5));
        let tab = browser.active_id();
        assert_eq!(attr(&browser, tab, "initial"), "visible");
        assert_eq!(browser.next_frame(), FrameRequest::Vsync);
        browser.new_tab();
        assert_eq!(attr(&browser, tab, "visibility"), "hidden");
        assert_eq!(attr(&browser, tab, "changes"), "1");
        assert!(browser.next_timer_deadline().is_some());
        let frames = attr(&browser, tab, "frames");
        for _ in 0..40 {
            browser.draw_frame(Viewport::new(800, 600, 1.0));
        }
        assert_eq!(
            attr(&browser, tab, "frames"),
            frames,
            "forty pumps must run no hidden animation callbacks"
        );
        assert_eq!(attr(&browser, tab, "timer"), "true");
        assert_ne!(browser.next_frame(), FrameRequest::Vsync);
        browser.move_tab(tab, 1);
        assert_eq!(
            attr(&browser, tab, "changes"),
            "1",
            "reordering preserves active identity"
        );
        browser.close_tab(0);
        assert_eq!(browser.active_id(), tab);
        assert_eq!(attr(&browser, tab, "visibility"), "visible");
        assert_eq!(attr(&browser, tab, "changes"), "2");
        assert_eq!(browser.next_frame(), FrameRequest::Vsync);
        browser.select_tab(0);
        assert_eq!(
            attr(&browser, tab, "changes"),
            "2",
            "selecting the same tab is not a transition"
        );
        browser.pump();
        assert_eq!(
            attr(&browser, tab, "frames").parse::<usize>().unwrap(),
            frames.parse::<usize>().unwrap() + 1
        );
        browser.new_tab();
        browser.select_tab(0);
        assert_eq!(
            attr(&browser, tab, "changes"),
            "4",
            "transitions are preserved even without a pump between commands"
        );
    });
}

#[test]
fn a_document_arriving_in_the_background_is_hidden_before_its_first_script() {
    on_content_stack(|| {
        let mut browser = Browser::new(Pages);
        browser.hold_requests(Some(Box::new(|_| true)));
        browser.navigate(URL);
        let tab = browser.active_id();
        browser.new_tab();
        browser.pump();
        let request = browser.held().first().unwrap().id;
        assert!(browser.fulfil_request(request, response(URL, HTML)));
        browser.wait_for_load(Duration::from_secs(5));
        assert_eq!(attr(&browser, tab, "initial"), "hidden");
        assert_eq!(attr(&browser, tab, "changes"), "0");
        assert_eq!(attr(&browser, tab, "frames"), "0");
        browser.select_tab(0);
        assert_eq!(attr(&browser, tab, "changes"), "1");
        assert_eq!(browser.next_frame(), FrameRequest::Vsync);
        browser.pump();
        assert_eq!(attr(&browser, tab, "frames"), "1");
    });
}

#[test]
fn a_parser_waiting_for_a_script_receives_visibility_changes_before_resuming() {
    on_content_stack(|| {
        let mut browser = Browser::new(Pages);
        browser.hold_requests(Some(Box::new(|_| true)));
        browser.navigate(URL);
        browser.pump();
        let request = browser
            .held()
            .iter()
            .find(|request| request.kind == crate::fetcher::ResourceKind::Document)
            .unwrap()
            .id;
        let html = format!(
            "{HTML}<script src='blocked.js'></script><script>state.setAttribute('after', document.visibilityState)</script>"
        );
        assert!(browser.fulfil_request(request, response(URL, &html)));
        browser.wait_for_load(Duration::from_secs(5));
        let tab = browser.active_id();
        assert_eq!(browser.readiness(0), Readiness::Interactive);
        assert_eq!(attr(&browser, tab, "initial"), "visible");
        browser.new_tab();
        assert_eq!(attr(&browser, tab, "visibility"), "hidden");
        assert_eq!(attr(&browser, tab, "changes"), "1");
        browser.pump();
        let script = browser
            .held()
            .iter()
            .find(|request| request.kind == crate::fetcher::ResourceKind::Script)
            .unwrap()
            .id;
        let mut external = response(
            "https://visibility.test/blocked.js",
            "state.setAttribute('external', document.visibilityState)",
        );
        external.content_type = Some("text/javascript".into());
        assert!(browser.fulfil_request(script, external));
        browser.wait_for_load(Duration::from_secs(5));
        assert_eq!(attr(&browser, tab, "external"), "hidden");
        assert_eq!(attr(&browser, tab, "after"), "hidden");
        assert_eq!(attr(&browser, tab, "frames"), "0");
        browser.select_tab(0);
        assert_eq!(attr(&browser, tab, "visibility"), "visible");
        browser.pump();
        assert_eq!(attr(&browser, tab, "frames"), "1");
    });
}
