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

/// A pointer that changes only how things look — a link's colour, a card's
/// border and shadow, the colour of what is inside a hovered card — is repainted
/// in place, with nothing laid out again, and what it draws is exactly what
/// restyling and laying out the whole page would have drawn.
#[test]
fn a_repaint_in_place_draws_what_a_whole_restyle_would() {
    let html = "<style>body { margin: 0 } a { color: blue; background: #eee } \
                a:hover { color: red; background: yellow } \
                .card { padding: 8px; border: 2px solid #ccc; margin: 4px } \
                .card:hover { border-color: green; box-shadow: 0 2px 6px rgba(0, 0, 0, .4) } \
                .card:hover .inner { color: purple }</style>\
                <div class=card><a href=#>first link</a><p class=inner>inner text</p></div>\
                <div class=card><a href=#>second link</a></div>\
                <p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p>\
                <p>i</p><p>j</p><p>k</p><p>l</p><p>m</p><p>n</p><p>o</p><p>p</p>";
    let (width, height) = (400_u32, 300_u32);
    let draw = |page: &mut PageScene, text: &mut otlyra_text::TextEngine| {
        let list = page.build_display_list(text, width as f32, height as f32, 0.0);
        let mut painter =
            otlyra_gfx::SkiaPainter::new_raster(width, height).expect("a raster surface");
        painter.clear(otlyra_gfx::peniko::Color::WHITE);
        otlyra_gfx::render_scrolled(&list, &mut painter, &page.offsets());
        painter.read_rgba8().expect("read back")
    };
    let parse = || otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;

    let mut text = otlyra_text::TextEngine::isolated();
    let mut page = PageScene::new(parse());
    draw(&mut page, &mut text);
    // Onto the page from outside it, which hovers the root and the body: that is
    // the whole document, and is restyled whole.
    page.pointer_moved(380.0, 280.0);
    draw(&mut page, &mut text);
    for (x, y) in [
        (30.0, 20.0),
        (30.0, 50.0),
        (30.0, 110.0),
        (380.0, 280.0),
        (30.0, 20.0),
    ] {
        page.pointer_moved(x, y);
        assert!(
            !page.damage().contains(otlyra_layout::Damage::LAYOUT),
            "at ({x}, {y}) the pointer asked for a relayout"
        );
        let in_place = draw(&mut page, &mut text);

        let mut whole = PageScene::new(parse());
        draw(&mut whole, &mut text);
        whole.pointer_moved(380.0, 280.0);
        whole.pointer_moved(x, y);
        whole.invalidate_layout();
        let restyled = draw(&mut whole, &mut text);
        assert!(
            in_place == restyled,
            "at ({x}, {y}) the page repainted in place differs from the page restyled whole"
        );
    }
}

/// Whatever path a hover takes — repainted in place, or the boxes built again
/// from styles restyled in part — the page drawn is the page a whole restyle
/// draws: a link that starts drawing a background, which changes what layout
/// makes of it, and a rule that reaches the next sibling.
#[test]
fn a_restyle_in_part_draws_what_a_whole_restyle_would() {
    let html = "<style>body { margin: 0 } a:hover { background: yellow } \
                .item:hover + .tip { color: red; font-weight: bold }</style>\
                <p><a href=#>a link</a> and text</p>\
                <div class=item>item</div><div class=tip>tip</div>\
                <p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p>\
                <p>i</p><p>j</p><p>k</p><p>l</p><p>m</p><p>n</p><p>o</p><p>p</p>";
    let (width, height) = (300_u32, 200_u32);
    let draw = |page: &mut PageScene, text: &mut otlyra_text::TextEngine| {
        let list = page.build_display_list(text, width as f32, height as f32, 0.0);
        let mut painter =
            otlyra_gfx::SkiaPainter::new_raster(width, height).expect("a raster surface");
        painter.clear(otlyra_gfx::peniko::Color::WHITE);
        otlyra_gfx::render_scrolled(&list, &mut painter, &page.offsets());
        painter.read_rgba8().expect("read back")
    };
    let parse = || otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;

    let mut text = otlyra_text::TextEngine::isolated();
    let mut page = PageScene::new(parse());
    draw(&mut page, &mut text);
    page.pointer_moved(290.0, 190.0);
    draw(&mut page, &mut text);
    for (x, y) in [(10.0, 20.0), (10.0, 45.0), (290.0, 190.0), (10.0, 45.0)] {
        page.pointer_moved(x, y);
        let in_part = draw(&mut page, &mut text);

        let mut whole = PageScene::new(parse());
        draw(&mut whole, &mut text);
        whole.pointer_moved(290.0, 190.0);
        whole.pointer_moved(x, y);
        whole.invalidate_layout();
        let restyled = draw(&mut whole, &mut text);
        assert!(
            in_part == restyled,
            "at ({x}, {y}) the page restyled in part differs from the page restyled whole"
        );
    }
}
