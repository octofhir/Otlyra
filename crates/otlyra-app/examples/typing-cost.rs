//! What one keystroke costs, from the event to the pixels it changed.
//!
//! Typing is the interaction with the least room: a keystroke that costs more
//! than a frame is felt as lag, and it is felt in the one place a person is
//! looking. This drives the window's own frame path — the same `compose` →
//! damage → retained-surface path the live window runs — so what it reports is
//! what a person waits for, not what an offscreen paint would have taken.
//!
//! Run with:
//!
//! ```text
//! cargo run --release -p otlyra-app --example typing-cost
//! ```

use std::time::{Duration, Instant};

use otlyra_app::browser::Browser;
use otlyra_app::fetcher::{Loaded, Loader};
use otlyra_app::ui::UI_HEIGHT;
use otlyra_platform::{Damage, FramePump, Key, Modifiers, PlatformEvent, Viewport};

/// What is typed, once per measured keystroke.
const TYPED: &str = "the quick brown fox jumps over the lazy dog";

/// A page with a text field and enough around it to be an ordinary document
/// rather than an empty one.
fn page(paragraphs: usize) -> String {
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><body style=\"margin:0\">\
         <input id=field style=\"position:absolute;left:40px;top:40px;\
         width:320px;height:32px;font-size:16px\">",
    );
    html.push_str("<div style=\"margin-top:100px\">");
    for index in 0..paragraphs {
        html.push_str(&format!(
            "<p>paragraph number {index}, with enough words in it to be shaped, \
             measured and laid out like the rest of a real document.</p>"
        ));
    }
    html.push_str("</div>");
    html
}

struct Pages(String);

impl Loader for Pages {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            content_type: Some("text/html".to_owned()),
            bytes: self.0.clone().into_bytes(),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

/// One measured keystroke: how long the browser took to answer the event, how
/// long it took to rebuild the frame's layers, how long the frame after it took,
/// and what that frame redrew.
struct Stroke {
    event: Duration,
    build: Duration,
    frame: Duration,
    damage: Damage,
    /// What the pipeline spent the keystroke on, by span.
    stages: Vec<(&'static str, f64)>,
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let at = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[at]
}

fn report(what: &str, strokes: &[Stroke], viewport: Viewport) {
    let mut events: Vec<f64> = strokes
        .iter()
        .map(|stroke| stroke.event.as_secs_f64() * 1000.0)
        .collect();
    let mut builds: Vec<f64> = strokes
        .iter()
        .map(|stroke| stroke.build.as_secs_f64() * 1000.0)
        .collect();
    let mut frames: Vec<f64> = strokes
        .iter()
        .map(|stroke| stroke.frame.as_secs_f64() * 1000.0)
        .collect();
    let mut totals: Vec<f64> = strokes
        .iter()
        .map(|stroke| (stroke.event + stroke.frame).as_secs_f64() * 1000.0)
        .collect();
    events.sort_by(f64::total_cmp);
    builds.sort_by(f64::total_cmp);
    frames.sort_by(f64::total_cmp);
    totals.sort_by(f64::total_cmp);

    let whole = u64::from(viewport.width) * u64::from(viewport.height);
    let mut areas: Vec<f64> = strokes
        .iter()
        .map(|stroke| match stroke.damage {
            Damage::Unchanged => 0.0,
            Damage::Region(rect) => {
                (u64::from(rect.width) * u64::from(rect.height)) as f64 / whole as f64 * 100.0
            }
            Damage::Full => 100.0,
        })
        .collect();
    areas.sort_by(f64::total_cmp);
    let average_area = areas.iter().sum::<f64>() / areas.len().max(1) as f64;
    let full = strokes
        .iter()
        .filter(|stroke| matches!(stroke.damage, Damage::Full))
        .count();

    println!(
        "{what}: {} keystrokes\n  \
         event  p50 {:6.2} ms  p95 {:6.2} ms\n  \
         build  p50 {:6.2} ms  p95 {:6.2} ms\n  \
         frame  p50 {:6.2} ms  p95 {:6.2} ms\n  \
         total  p50 {:6.2} ms  p95 {:6.2} ms\n  \
         damage p50 {:5.1}%  p95 {:5.1}%  average {:5.1}%, \
         {full} whole-surface frames",
        strokes.len(),
        percentile(&events, 0.5),
        percentile(&events, 0.95),
        percentile(&builds, 0.5),
        percentile(&builds, 0.95),
        percentile(&frames, 0.5),
        percentile(&frames, 0.95),
        percentile(&totals, 0.5),
        percentile(&totals, 0.95),
        percentile(&areas, 0.5),
        percentile(&areas, 0.95),
        average_area,
    );

    let mut names: Vec<&'static str> = strokes
        .iter()
        .flat_map(|stroke| stroke.stages.iter().map(|(span, _)| *span))
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut lines: Vec<(f64, String)> = names
        .into_iter()
        .map(|span| {
            let mut took: Vec<f64> = strokes
                .iter()
                .map(|stroke| {
                    stroke
                        .stages
                        .iter()
                        .find(|(name, _)| *name == span)
                        .map_or(0.0, |(_, took)| *took)
                })
                .collect();
            took.sort_by(f64::total_cmp);
            let median = percentile(&took, 0.5);
            (median, format!("    {span:<20} p50 {median:6.2} ms"))
        })
        .collect();
    lines.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (took, line) in lines {
        if took >= 0.005 {
            println!("{line}");
        }
    }
}

/// Type one character and draw the frame it asks for.
fn stroke(pump: &mut FramePump, browser: &mut Browser, character: char) -> Stroke {
    otlyra_app::observability::journal().clear();
    let started = Instant::now();
    pump.event(
        browser,
        PlatformEvent::KeyPressed {
            key: Key::Character(character),
            modifiers: Modifiers::default(),
        },
    );
    pump.event(browser, PlatformEvent::TextInput(character));
    let event = started.elapsed();

    // Compose once here, off the pump, to separate rebuilding the layers —
    // restyle, layout, shaping, display list — from rasterizing the damage. The
    // pump's own frame then composes a second time, which is served from the
    // caches, so what it adds is the raster and the readback.
    let started = Instant::now();
    let _ = otlyra_platform::Painter::compose(browser, pump.viewport());
    let build = started.elapsed();

    let started = Instant::now();
    pump.frame(browser).expect("a frame");
    Stroke {
        event,
        build,
        frame: started.elapsed(),
        damage: pump.damage(),
        stages: stages(),
    }
}

fn press(pump: &mut FramePump, browser: &mut Browser, x: f64, y: f64) {
    pump.event(browser, PlatformEvent::PointerMoved { x, y });
    pump.event(browser, PlatformEvent::PointerPressed { clicks: 1 });
    pump.event(browser, PlatformEvent::PointerReleased);
    pump.frame(browser).expect("a frame");
}

/// The pipeline stages the last frame spent time in, longest first.
///
/// Read from the journal the browser already keeps, so the attribution is the
/// same one the inspector and `otlyra:frameTimings` report rather than a second
/// set of timings that can disagree with them.
fn stages() -> Vec<(&'static str, f64)> {
    let mut stages: Vec<(&'static str, f64)> = otlyra_app::observability::journal()
        .latest()
        .into_iter()
        .map(|timing| (timing.span, timing.took.as_secs_f64() * 1000.0))
        .collect();
    stages.sort_by(|a, b| b.1.total_cmp(&a.1));
    stages
}

fn main() {
    otlyra_app::observability::init();
    let paragraphs: usize = std::env::args()
        .nth(1)
        .and_then(|count| count.parse().ok())
        .unwrap_or(200);
    let viewport = Viewport::new(2048, 1536, 2.0);

    let mut browser = Browser::new(Pages(page(paragraphs)));
    browser.navigate("https://typing.example/");
    browser.wait_for_load(Duration::from_secs(10));
    browser.prepare_frame(viewport, Duration::from_secs(10));

    let mut pump = FramePump::new(viewport);
    pump.open(&mut browser).expect("a first frame");

    println!(
        "a page of {paragraphs} paragraphs at {}x{}",
        viewport.width, viewport.height
    );

    // The address field, which is browser chrome and touches no document.
    press(&mut pump, &mut browser, 400.0, UI_HEIGHT - 20.0);
    let chrome: Vec<Stroke> = TYPED
        .chars()
        .map(|character| stroke(&mut pump, &mut browser, character))
        .collect();
    report("omnibox", &chrome, viewport);

    // And the page's own field, which is the document.
    press(&mut pump, &mut browser, 200.0, UI_HEIGHT + 56.0);
    let field: Vec<Stroke> = TYPED
        .chars()
        .map(|character| stroke(&mut pump, &mut browser, character))
        .collect();
    report("page field", &field, viewport);

    // A scroll, for scale: it rebuilds the page's display list and touches
    // neither style nor layout, so what it costs is the display list alone.
    pump.event(
        &mut browser,
        PlatformEvent::PointerMoved {
            x: 600.0,
            y: UI_HEIGHT + 400.0,
        },
    );
    pump.frame(&mut browser).expect("a frame");
    let scrolls: Vec<Stroke> = (0..TYPED.chars().count())
        .map(|_| {
            let started = Instant::now();
            pump.event(
                &mut browser,
                PlatformEvent::Scroll {
                    x: 0.0,
                    y: 40.0,
                    source: otlyra_platform::ScrollSource::Wheel,
                    modifiers: otlyra_platform::Modifiers::default(),
                },
            );
            let event = started.elapsed();
            let started = Instant::now();
            let _ = otlyra_platform::Painter::compose(&mut browser, pump.viewport());
            let build = started.elapsed();
            let started = Instant::now();
            pump.frame(&mut browser).expect("a frame");
            Stroke {
                event,
                build,
                frame: started.elapsed(),
                damage: pump.damage(),
                stages: stages(),
            }
        })
        .collect();
    report("scroll (display list only)", &scrolls, viewport);
}
