//! What a frame costs, scenario by scenario, split into the phases that make it.
//!
//! Scrolling with a wheel and with a trackpad, the pointer sweeping over the page
//! and over the toolbar, and the window being resized — each drawn through the
//! window's own frame path (`compose` → damage → retained surface) and timed as
//! event, compose (with the style, layout and display-list stages inside it),
//! raster and read-back. It also counts what the page's list asks the rasterizer
//! for, because that is mostly what decides a frame's cost: how many offscreen
//! groups, how many blurs.
//!
//! ```text
//! cargo run --release -p otlyra-app --example repaint-cost -- ~/otlyra-mirrors/github/index.html
//! cargo run --release -p otlyra-app --example repaint-cost -- gen:400 1440x900@2
//! cargo run --release -p otlyra-app --example repaint-cost -- gen:400:flat
//! ```
//!
//! `gen:N` is N cards with rounded corners, shadows and a hovered link;
//! `gen:N:flat` is the same without the shadows. Take the numbers on an idle
//! machine: the loops are wall-clock and anything else running lands in them.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use otlyra_app::browser::Browser;
use otlyra_app::fetcher::{Loaded, Loader};
use otlyra_app::ui::UI_HEIGHT;
use otlyra_gfx::kurbo::{Rect, Shape};
use otlyra_gfx::{DisplayItem, SkiaPainter};
use otlyra_platform::{
    Damage, FramePump, Modifiers, Painter as _, PlatformEvent, ScrollSource, Viewport,
};

/// Serves a generated page, or files straight from disk.
enum Pages {
    Generated(String),
    Files,
}

impl Loader for Pages {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        let bytes = match self {
            Self::Generated(html) if url.starts_with("https://gen.example/") => {
                html.clone().into_bytes()
            }
            Self::Generated(_) => return Err("nothing else is generated".to_owned()),
            Self::Files => {
                let path = url::Url::parse(url)
                    .map_err(|error| error.to_string())?
                    .to_file_path()
                    .map_err(|()| format!("{url} is not a file"))?;
                std::fs::read(&path).map_err(|error| error.to_string())?
            }
        };
        let content_type = match url.rsplit('.').next() {
            Some("css") => "text/css",
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("svg") => "image/svg+xml",
            Some("js") => "text/javascript",
            Some("woff2") => "font/woff2",
            _ => "text/html",
        };
        Ok(Loaded {
            content_type: Some(content_type.to_owned()),
            bytes,
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

fn generated(cards: usize, shadows: bool) -> String {
    let shadow = if shadows {
        "box-shadow:0 1px 3px rgba(0,0,0,.2)"
    } else {
        ""
    };
    let mut html = format!(
        "<!doctype html><meta charset=utf-8><style>a:hover{{color:red}}\
         .card{{border:1px solid #ccc;border-radius:8px;padding:12px;margin:12px;{shadow}}}\
         </style><body>"
    );
    for index in 0..cards {
        html.push_str(&format!(
            "<div class=card><h3>Card {index}</h3><p>Paragraph {index} with \
             <a href=#{index}>a link</a> and enough words in it to wrap across a line \
             or two of an ordinary window, like the rest of a real document does.</p></div>"
        ));
    }
    html
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * fraction).round() as usize]
}

/// The pipeline stage `name` took in the last frame, from the browser's journal.
fn stage(name: &str) -> f64 {
    otlyra_app::observability::journal()
        .latest()
        .into_iter()
        .filter(|timing| timing.span == name)
        .map(|timing| ms(timing.took))
        .sum()
}

/// One scenario's frames.
#[derive(Default)]
struct Row {
    event: Vec<f64>,
    compose: Vec<f64>,
    style: Vec<f64>,
    layout: Vec<f64>,
    list: Vec<f64>,
    raster: Vec<f64>,
    readback: Vec<f64>,
    total: Vec<f64>,
    damage: Vec<f64>,
}

impl Row {
    fn report(&self, name: &str) {
        let over = |budget: f64| self.total.iter().filter(|took| **took > budget).count();
        println!(
            "{name:<26} n={:<3} total p50 {:7.2} p95 {:7.2} | event {:5.2} compose {:6.2} \
             (style {:6.2} layout {:6.2} list {:5.2}) raster {:7.2} readback {:5.2} | \
             damage {:5.1}% | >8.3ms {} >16.7ms {}",
            self.total.len(),
            percentile(&self.total, 0.5),
            percentile(&self.total, 0.95),
            percentile(&self.event, 0.5),
            percentile(&self.compose, 0.5),
            percentile(&self.style, 0.5),
            percentile(&self.layout, 0.5),
            percentile(&self.list, 0.5),
            percentile(&self.raster, 0.5),
            percentile(&self.readback, 0.5),
            percentile(&self.damage, 0.5),
            over(8.33),
            over(16.67),
        );
    }
}

/// The window's frame path with raster and read-back timed on their own.
///
/// The pump decides the damage exactly as the window does; the frame is then
/// replayed into a surface of this harness's own under that damage, which is
/// where raster and read-back are timed.
struct Frames {
    pump: FramePump,
    surface: SkiaPainter,
}

impl Frames {
    fn event(&mut self, browser: &mut Browser, event: PlatformEvent) -> Duration {
        otlyra_app::observability::journal().clear();
        let started = Instant::now();
        self.pump.event(browser, event);
        started.elapsed()
    }

    fn draw(&mut self, browser: &mut Browser, row: &mut Row, event: Duration) {
        let viewport = self.pump.viewport();
        let started = Instant::now();
        let scene = browser
            .compose(viewport)
            .expect("the interface draws layers");
        let compose = started.elapsed();
        row.style.push(stage("recalc_style"));
        row.layout.push(stage("layout"));
        row.list.push(stage("build_display_list"));

        // The pump's own compose is served from the caches.
        self.pump.frame(browser).expect("a frame");
        let whole = f64::from(viewport.width) * f64::from(viewport.height);
        let damage = match self.pump.damage() {
            Damage::Unchanged => None,
            Damage::Full => Some(Rect::new(
                0.0,
                0.0,
                f64::from(viewport.width),
                f64::from(viewport.height),
            )),
            Damage::Region(rect) => Some(Rect::new(
                f64::from(rect.x),
                f64::from(rect.y),
                f64::from(rect.x + rect.width),
                f64::from(rect.y + rect.height),
            )),
        };

        let (mut raster, mut readback) = (Duration::ZERO, Duration::ZERO);
        if let Some(clip) = damage {
            let started = Instant::now();
            self.surface.clip_to(clip);
            self.surface.clear_rect(clip);
            for layer in &scene.layers {
                otlyra_gfx::render(&layer.list, &mut self.surface);
            }
            self.surface.reset_clip();
            raster = started.elapsed();
            let started = Instant::now();
            self.surface
                .read_rgba8_rect(
                    clip.x0 as u32,
                    clip.y0 as u32,
                    clip.width() as u32,
                    clip.height() as u32,
                )
                .expect("read back");
            readback = started.elapsed();
        }
        row.event.push(ms(event));
        row.compose.push(ms(compose));
        row.raster.push(ms(raster));
        row.readback.push(ms(readback));
        row.total.push(ms(event + compose + raster + readback));
        row.damage
            .push(damage.map_or(0.0, |rect| rect.area() / whole * 100.0));
    }
}

/// What the frame's lists ask the rasterizer for.
fn census(browser: &mut Browser, viewport: Viewport) {
    let scene = browser.compose(viewport).expect("layers");
    let screen = Rect::new(
        0.0,
        0.0,
        f64::from(viewport.width),
        f64::from(viewport.height),
    );
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    let (mut groups, mut group_area) = (0, 0.0);
    for layer in &scene.layers {
        for item in layer.list.items() {
            let kind = match item {
                DisplayItem::PushLayer { alpha, clip, .. } => {
                    if *alpha < 1.0 {
                        groups += 1;
                        group_area += clip.bounding_box().intersect(screen).area() / screen.area();
                    }
                    "layer"
                }
                DisplayItem::PopLayer => continue,
                DisplayItem::Blurred { .. } => "blur",
                DisplayItem::Fill { .. } => "fill",
                DisplayItem::Stroke { .. } => "stroke",
                DisplayItem::Glyphs { .. } => "glyphs",
                DisplayItem::Image { .. } => "image",
                DisplayItem::HitTest { .. } => "hit test",
            };
            *kinds.entry(kind).or_default() += 1;
        }
    }
    println!("items {kinds:?}; translucent groups {groups} covering {group_area:.1} viewports");
}

fn main() {
    otlyra_app::observability::init();
    let page = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "gen:400".to_owned());
    let size = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "1440x900@2".to_owned());
    let (dimensions, scale) = size.split_once('@').expect("WxH@scale");
    let (width, height) = dimensions.split_once('x').expect("WxH@scale");
    let scale: f64 = scale.parse().expect("a scale");
    let device = |logical: &str| (logical.parse::<f64>().expect("a size") * scale) as u32;
    let viewport = Viewport::new(device(width), device(height), scale);

    let (loader, url) = match page.strip_prefix("gen:") {
        Some(spec) => {
            let (count, flat) = match spec.split_once(':') {
                Some((count, "flat")) => (count, true),
                _ => (spec, false),
            };
            let count = count.parse().expect("gen:N");
            (
                Pages::Generated(generated(count, !flat)),
                "https://gen.example/".to_owned(),
            )
        }
        None => {
            let path = std::fs::canonicalize(&page).expect("the page exists");
            (
                Pages::Files,
                url::Url::from_file_path(path)
                    .expect("an absolute path")
                    .to_string(),
            )
        }
    };

    let mut browser = Browser::new(loader);
    let started = Instant::now();
    browser.navigate(&url);
    browser.wait_for_load(Duration::from_secs(30));
    browser.prepare_frame(viewport, Duration::from_secs(30));
    println!(
        "{page} at {}x{}, loaded in {:.0} ms",
        viewport.width,
        viewport.height,
        ms(started.elapsed())
    );

    let mut pump = FramePump::new(viewport);
    pump.open(&mut browser).expect("a first frame");
    let mut frames = Frames {
        pump,
        surface: SkiaPainter::new_raster(viewport.width, viewport.height).expect("a surface"),
    };
    census(&mut browser, viewport);
    // Warm the surface's caches with one whole frame.
    frames.draw(&mut browser, &mut Row::default(), Duration::ZERO);
    let width = viewport.logical_width();

    let mut row = Row::default();
    for _ in 0..20 {
        otlyra_app::observability::journal().clear();
        frames.draw(&mut browser, &mut row, Duration::ZERO);
    }
    row.report("idle");

    let parked = frames.event(
        &mut browser,
        PlatformEvent::PointerMoved {
            x: width / 2.0,
            y: UI_HEIGHT + 300.0,
        },
    );
    frames.draw(&mut browser, &mut Row::default(), parked);

    let scroll = |y: f64, source| PlatformEvent::Scroll {
        x: 0.0,
        y,
        source,
        modifiers: Modifiers::default(),
    };
    let mut row = Row::default();
    for _ in 0..40 {
        let event = frames.event(&mut browser, scroll(40.0, ScrollSource::Wheel));
        frames.draw(&mut browser, &mut row, event);
    }
    row.report("wheel, 40 px a frame");

    let mut row = Row::default();
    for _ in 0..40 {
        let mut event = Duration::ZERO;
        for _ in 0..8 {
            event += frames.event(&mut browser, scroll(2.0, ScrollSource::Trackpad));
        }
        frames.draw(&mut browser, &mut row, event);
    }
    row.report("trackpad, 8 x 2 px a frame");

    let mut row = Row::default();
    let mut restyled = 0;
    for step in 0..60_u32 {
        let event = frames.event(
            &mut browser,
            PlatformEvent::PointerMoved {
                x: 60.0 + f64::from(step % 20) * 30.0,
                y: UI_HEIGHT + 20.0 + f64::from(step) * 7.0,
            },
        );
        frames.draw(&mut browser, &mut row, event);
        if row.style.last().is_some_and(|took| *took > 0.0) {
            restyled += 1;
        }
    }
    row.report(&format!("hover sweep ({restyled}/60 styled)"));

    let mut row = Row::default();
    for step in 0..30_u32 {
        let event = frames.event(
            &mut browser,
            PlatformEvent::PointerMoved {
                x: 20.0 + f64::from(step) * 25.0,
                y: UI_HEIGHT - 20.0,
            },
        );
        frames.draw(&mut browser, &mut row, event);
    }
    row.report("hover over the toolbar");

    let mut row = Row::default();
    for step in 0..20_u32 {
        let narrower = if step % 2 == 0 {
            (8.0 * scale) as u32
        } else {
            0
        };
        let resized = Viewport::new(viewport.width - narrower, viewport.height, scale);
        otlyra_app::observability::journal().clear();
        let started = Instant::now();
        frames.pump.resize(&mut browser, resized);
        let event = started.elapsed();
        frames.surface = SkiaPainter::new_raster(resized.width, resized.height).expect("a surface");
        frames.draw(&mut browser, &mut row, event);
    }
    row.report("resize, 8 px a frame");
}
