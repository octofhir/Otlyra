//! Renders a frame through the real pipeline and compares it to a committed PNG.
//!
//! This is the test that catches "the rectangles are fine but the text stopped
//! appearing", which no unit test over paint operations can see.
//!
//! The scene shapes against the repo-vendored font with system fonts disabled, so
//! the reference is machine-independent. Regenerate it deliberately:
//!
//! ```sh
//! OTLYRA_UPDATE_GOLDEN=1 cargo test -p otlyra-app --test golden_image
//! ```

use std::path::PathBuf;

use otlyra_app::scene::DemoScene;
use otlyra_platform::{Viewport, render_offscreen};

/// Per-channel tolerance. Rasterization is deterministic for identical input on
/// identical Skia, so this is tight on purpose: it absorbs nothing but a
/// last-bit rounding difference.
const TOLERANCE: u8 = 1;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("goldens")
        .join("demo-scene.png")
}

fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().expect("valid PNG header");
    let mut buffer = vec![0; reader.output_buffer_size().expect("known buffer size")];
    let info = reader.next_frame(&mut buffer).expect("valid PNG body");
    buffer.truncate(info.buffer_size());
    (info.width, info.height, buffer)
}

#[test]
fn the_demo_scene_matches_its_golden_image() {
    let viewport = Viewport::new(800, 600, 2.0);
    let mut scene = DemoScene::new();
    let rendered = render_offscreen(&mut scene, viewport).expect("render a frame");

    let path = golden_path();

    if std::env::var_os("OTLYRA_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("goldens directory")).expect("create dir");
        std::fs::write(&path, &rendered).expect("write golden");
        eprintln!("updated golden: {}", path.display());
        return;
    }

    let expected = std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "missing golden {}: {error}\nregenerate with OTLYRA_UPDATE_GOLDEN=1",
            path.display()
        )
    });

    let (golden_width, golden_height, golden_pixels) = decode(&expected);
    let (width, height, pixels) = decode(&rendered);

    assert_eq!(
        (width, height),
        (golden_width, golden_height),
        "rendered frame is {width}x{height}, golden is {golden_width}x{golden_height}"
    );

    let mut differing = 0usize;
    let mut worst = 0u8;
    let mut worst_at = (0u32, 0u32);

    for (index, (actual, expected)) in pixels.iter().zip(golden_pixels.iter()).enumerate() {
        let delta = actual.abs_diff(*expected);
        if delta > worst {
            worst = delta;
            let pixel = index / 4;
            worst_at = (pixel as u32 % width, pixel as u32 / width);
        }
        if delta > TOLERANCE {
            differing += 1;
        }
    }

    // No allowance for a number of differing pixels: a tolerance that also permits
    // a pixel count is two knobs where one will do, and it is the second one that
    // lets real regressions through.
    assert_eq!(
        differing, 0,
        "{differing} channel samples differ by more than {TOLERANCE}; \
         worst delta {worst} at ({}, {})\nregenerate with OTLYRA_UPDATE_GOLDEN=1 if intended",
        worst_at.0, worst_at.1
    );
}

/// The frame must not be blank. A golden test passes happily against two identical
/// white rectangles, so this asserts separately that something was actually drawn.
#[test]
fn the_rendered_frame_is_not_blank() {
    let viewport = Viewport::new(400, 300, 1.0);
    let mut scene = DemoScene::new();
    let rendered = render_offscreen(&mut scene, viewport).expect("render a frame");
    let (_, _, pixels) = decode(&rendered);

    let distinct: std::collections::BTreeSet<[u8; 4]> = pixels
        .chunks_exact(4)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect();

    assert!(
        distinct.len() > 16,
        "expected a rendered scene, found {} distinct colours",
        distinct.len()
    );
}
