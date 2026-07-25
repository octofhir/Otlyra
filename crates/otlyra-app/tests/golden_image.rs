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

mod common;

use otlyra_app::scene::DemoScene;
use otlyra_platform::{Viewport, render_offscreen};

#[test]
fn the_demo_scene_matches_its_golden_image() {
    let viewport = Viewport::new(800, 600, 2.0);
    let mut scene = DemoScene::new();
    let rendered = render_offscreen(&mut scene, viewport).expect("render a frame");

    common::assert_matches_golden(&rendered, &common::goldens_dir().join("demo-scene.png"));
}

/// The frame must not be blank. A golden test passes happily against two identical
/// white rectangles, so this asserts separately that something was actually drawn.
#[test]
fn the_rendered_frame_is_not_blank() {
    let viewport = Viewport::new(400, 300, 1.0);
    let mut scene = DemoScene::new();
    let rendered = render_offscreen(&mut scene, viewport).expect("render a frame");
    let (_, _, pixels) = common::decode(&rendered);

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
