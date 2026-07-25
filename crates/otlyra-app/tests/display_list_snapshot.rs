//! Snapshot of the display list the demo scene builds.
//!
//! This is the primary regression seam for everything above the rasterizer: it
//! asserts *what* the frame draws, in order, without a GPU, a font raster or an
//! image comparison. A change here is a change in what the browser decided to
//! paint, which is nearly always worth reading.
//!
//! Review and accept changes with `cargo insta review`.

use otlyra_app::scene::DemoScene;
use otlyra_platform::Viewport;

#[test]
fn the_demo_scene_display_list_is_stable() {
    let mut scene = DemoScene::new();
    let list = scene.build_display_list(Viewport::new(800, 600, 2.0));

    insta::assert_debug_snapshot!(list);
}

/// The list must survive a round trip through JSON unchanged. This is the property
/// that makes a renderer process possible later without redesigning the list.
#[test]
fn the_display_list_round_trips_through_json() {
    let mut scene = DemoScene::new();
    let list = scene.build_display_list(Viewport::new(400, 300, 1.0));

    let json = serde_json::to_string(&list).expect("serialize");
    let decoded: otlyra_gfx::DisplayList = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(decoded, list);
}

/// Hit regions must line up with the swatches that were drawn. This is the
/// assertion that catches a link becoming clickable somewhere other than where it
/// appears.
#[test]
fn every_swatch_is_hit_testable_where_it_is_drawn() {
    let mut scene = DemoScene::new();
    let viewport = Viewport::new(800, 600, 2.0);
    let list = scene.build_display_list(viewport);

    let hit_regions: Vec<_> = list
        .items()
        .iter()
        .filter_map(|item| match item {
            otlyra_gfx::DisplayItem::HitTest {
                rect,
                transform,
                id,
            } => Some((*rect, *transform, *id)),
            _ => None,
        })
        .collect();

    assert_eq!(hit_regions.len(), 4, "one hit region per swatch");

    for (rect, transform, id) in hit_regions {
        let centre = transform * rect.center();
        assert_eq!(
            list.hit_test(centre),
            Some(id),
            "the centre of {id:?} should hit it"
        );
    }
}
