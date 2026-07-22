//! How long the rasterizer takes to replay one display list, over and over.
//!
//! The first replay of a process pays for everything Skia builds lazily — the
//! font manager, the glyph caches, the raster pipeline — and a browser pays that
//! once. What a page costs *while it is being read* is the twentieth replay, and
//! the two are worth telling apart before either is called slow.
//!
//! ```text
//! cargo run --release -p otlyra-gfx --example raster-cost
//! ```

use otlyra_gfx::kurbo::{Affine, Rect, Shape};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList, PaintTarget as _, SkiaPainter, render};

/// How many frames after the first to time.
const FRAMES: usize = 20;

fn main() {
    let (width, height) = (1200u32, 900u32);
    let list = page(width, height);
    println!("{} items", list.len());

    let mut painter = SkiaPainter::new_raster(width, height).expect("a raster surface");

    let started = std::time::Instant::now();
    painter.reset();
    render(&list, &mut painter);
    println!("first replay:  {:?}", started.elapsed());

    let started = std::time::Instant::now();
    for _ in 0..FRAMES {
        painter.reset();
        render(&list, &mut painter);
    }
    let each = started.elapsed() / FRAMES as u32;
    println!("steady replay: {each:?} per frame");

    let started = std::time::Instant::now();
    let png = painter.encode_png().expect("encodes");
    println!(
        "encode png:    {:?} ({} bytes)",
        started.elapsed(),
        png.len()
    );
}

/// A page's worth of drawing: boxes, borders and text-sized fills.
fn page(width: u32, height: u32) -> DisplayList {
    let mut list = DisplayList::new();
    let fill = |list: &mut DisplayList, rect: Rect, colour: Color| {
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Solid(colour),
            brush_transform: None,
            shape: rect.to_path(0.1),
        });
    };

    fill(
        &mut list,
        Rect::new(0.0, 0.0, f64::from(width), f64::from(height)),
        Color::WHITE,
    );
    for row in 0..30 {
        let y = f64::from(row) * 30.0;
        fill(
            &mut list,
            Rect::new(8.0, y, f64::from(width) - 8.0, y + 26.0),
            Color::from_rgb8(0xF7, 0xF7, 0xF9),
        );
        for word in 0..12 {
            let x = 16.0 + f64::from(word) * 90.0;
            fill(
                &mut list,
                Rect::new(x, y + 6.0, x + 70.0, y + 20.0),
                Color::from_rgb8(0x20, 0x20, 0x28),
            );
        }
    }
    list
}
