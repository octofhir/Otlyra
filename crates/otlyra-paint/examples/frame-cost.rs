//! What a page costs, phase by phase, on the first frame and on every frame after.
//!
//! The first frame of a process pays for everything built lazily — the font
//! collection, the shaper's caches, the rasterizer's glyph atlas — and a browser
//! pays that once per session. What decides whether a page is pleasant to *use* is
//! the second frame and the twentieth, and the two are worth telling apart before
//! either is called slow.
//!
//! ```text
//! cargo run --release -p otlyra-paint --example frame-cost -- page.html
//! ```
//!
//! With no argument it generates a page of four hundred cards, which is about what
//! a long article or a busy dashboard comes to.

use std::time::Instant;

use otlyra_layout::{Viewport, build_styled_box_tree, layout};
use otlyra_text::TextEngine;

/// How many frames after the first to time.
const FRAMES: usize = 10;
/// The viewport every phase is measured at.
const SIZE: (f32, f32) = (1200.0, 900.0);

fn main() {
    let source = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(&path).expect("the page can be read"),
        None => generated(),
    };
    println!("{} KB of markup", source.len() / 1024);

    let parse = |source: &str| otlyra_html::parse(source.as_bytes(), Some("utf-8"));
    let viewport = otlyra_css::cascade::Viewport {
        width: SIZE.0,
        height: SIZE.1,
        scale: 1.0,
        text_scale: 1.0,
        color_scheme: Default::default(),
    };

    let started = Instant::now();
    let parsed = parse(&source);
    let parse_first = started.elapsed();
    let started = Instant::now();
    for _ in 0..FRAMES {
        std::hint::black_box(parse(&source));
    }
    println!(
        "parse        first {:>9.2?}   steady {:>9.2?}",
        parse_first,
        started.elapsed() / FRAMES as u32
    );

    // The cascade, kept between restyles the way a page keeps it.
    let started = Instant::now();
    let mut styler = otlyra_css::cascade::Styler::new(
        &parsed.document,
        viewport,
        &otlyra_css::cascade::ExternalSheets::default(),
    );
    let styles = styler.style(&parsed.document);
    let style_first = started.elapsed();
    let started = Instant::now();
    for _ in 0..FRAMES {
        std::hint::black_box(styler.style(&parsed.document));
    }
    println!(
        "style        first {:>9.2?}   steady {:>9.2?}",
        style_first,
        started.elapsed() / FRAMES as u32
    );

    let started = Instant::now();
    let mut boxes = build_styled_box_tree(&parsed.document, &styles);
    println!("box tree     first {:>9.2?}", started.elapsed());

    let mut text = TextEngine::new();
    let size = Viewport {
        width: SIZE.0,
        height: SIZE.1,
    };
    let started = Instant::now();
    let fragments = layout(&mut boxes, &mut text, size);
    let layout_first = started.elapsed();
    let started = Instant::now();
    for _ in 0..FRAMES {
        std::hint::black_box(layout(&mut boxes, &mut text, size));
    }
    println!(
        "layout       first {:>9.2?}   steady {:>9.2?}",
        layout_first,
        started.elapsed() / FRAMES as u32
    );

    let frame = otlyra_paint::Frame {
        viewport: SIZE,
        ..otlyra_paint::Frame::default()
    };
    let started = Instant::now();
    let list = otlyra_paint::build_display_list_with(&fragments, &frame);
    let list_first = started.elapsed();
    let started = Instant::now();
    for _ in 0..FRAMES {
        std::hint::black_box(otlyra_paint::build_display_list_with(&fragments, &frame));
    }
    println!(
        "display list first {:>9.2?}   steady {:>9.2?}   ({} items)",
        list_first,
        started.elapsed() / FRAMES as u32,
        list.len()
    );

    let mut painter = otlyra_gfx::SkiaPainter::new_raster(SIZE.0 as u32, SIZE.1 as u32)
        .expect("a raster surface");
    let started = Instant::now();
    {
        use otlyra_gfx::PaintTarget as _;
        painter.reset();
    }
    otlyra_gfx::render(&list, &mut painter);
    let raster_first = started.elapsed();
    let started = Instant::now();
    for _ in 0..FRAMES {
        use otlyra_gfx::PaintTarget as _;
        painter.reset();
        otlyra_gfx::render(&list, &mut painter);
    }
    println!(
        "rasterize    first {:>9.2?}   steady {:>9.2?}",
        raster_first,
        started.elapsed() / FRAMES as u32
    );

    // Scrolling is the frame a reader makes most of: no restyle, no layout, a new
    // display list for the part now on screen, and a repaint.
    let scrolled = otlyra_paint::Frame {
        viewport: SIZE,
        scroll_y: 400.0,
        ..otlyra_paint::Frame::default()
    };
    let started = Instant::now();
    for _ in 0..FRAMES {
        use otlyra_gfx::PaintTarget as _;
        let list = otlyra_paint::build_display_list_with(&fragments, &scrolled);
        painter.reset();
        otlyra_gfx::render(&list, &mut painter);
    }
    println!(
        "scroll frame               steady {:>9.2?}",
        started.elapsed() / FRAMES as u32
    );
}

/// A page of four hundred cards: headings, prose with inline elements, a flex row
/// and a small grid in each.
fn generated() -> String {
    let mut out = String::from(
        "<!doctype html><meta charset=utf-8><style>\
         body{margin:0;font:14px/1.4 Times}\
         .card{border:1px solid #ccc;padding:8px;margin:6px;background:#f7f7f9}\
         .row{display:flex;gap:8px} .row>div{flex:1}\
         .g{display:grid;grid-template-columns:repeat(4,1fr);gap:6px}\
         </style><body>",
    );
    for index in 0..400 {
        out.push_str(&format!(
            "<div class=card><h3>Heading {index}</h3>\
             <p>Some prose with <b>bold</b>, <i>italic</i> and a <a href=#>link</a> in it, \
             long enough to wrap across a couple of lines on an ordinary window width.</p>\
             <div class=row><div>left {index}</div><div>middle</div><div>right</div></div>\
             <div class=g><div>a</div><div>b</div><div>c</div><div>d</div></div></div>"
        ));
    }
    out
}
