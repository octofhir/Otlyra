//! Rendering tests: what the pipeline puts on a surface, read back as pixels.
//!
//! A dump says what style an element computed; only pixels say whether the
//! difference reached the screen.

use super::*;

/// How many non-white pixels each row of the rendered page has.
fn ink_per_row(html: &str, width: u32, height: u32) -> Vec<u32> {
    let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
    let mut page = PageScene::new(document);
    // System fonts, not the vendored one: the vendored family has a single
    // static face, and a weight that no face can express is a weight no
    // rendering bug can be seen in.
    let mut text = otlyra_text::TextEngine::new();
    let list = page.build_display_list(&mut text, width as f32, height as f32, 0.0);

    let mut painter = otlyra_gfx::SkiaPainter::new_raster(width, height).expect("a raster surface");
    painter.clear(otlyra_gfx::peniko::Color::WHITE);
    otlyra_gfx::render(&list, &mut painter);
    let pixels = painter.read_rgba8().expect("read back");

    (0..height)
        .map(|y| {
            (0..width)
                .filter(|x| {
                    let i = ((y * width + x) * 4) as usize;
                    pixels[i] < 200
                })
                .count() as u32
        })
        .collect()
}

/// Bold and regular of a variable font share one font file, so anything that
/// caches by file alone hands the second run the first run's weight. The only
/// way to see that is to draw both and count the ink.
#[test]
fn bold_text_is_heavier_than_regular_text_in_the_same_frame() {
    let ink = ink_per_row(
        "<style>p { font-size: 40px; margin: 0 } .b { font-weight: 700 }</style>\
         <p>iiiiiiii</p><p class=\"b\">iiiiiiii</p>",
        400,
        120,
    );

    let half = ink.len() / 2;
    let regular: u32 = ink[..half].iter().sum();
    let bold: u32 = ink[half..].iter().sum();
    assert!(regular > 0, "the regular line drew nothing");
    assert!(
        bold > regular,
        "bold inked {bold} pixels and regular {regular}"
    );
}
