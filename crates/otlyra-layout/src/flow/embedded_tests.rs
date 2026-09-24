//! Embedded content, checked against whole documents: HTML's replaced elements
//! (§15.4) at the size their attributes, their content and the default object
//! size give them, and the fallback inside them shown only where HTML says.

use super::tests::{boxes_of, image_rect, laid_out, laid_out_with_image, picture, rect_of};
use crate::{FragmentKind, FragmentTree};

/// A border box's width and height.
fn size_of(html: &str, tag: &str) -> (f32, f32) {
    let (tree, boxes) = laid_out(&format!("<body style='margin: 0'>{html}"), 800.0);
    let rect = rect_of(&tree, &boxes, tag);
    (rect.width, rect.height)
}

/// Whether any run of text on the page holds `word`.
fn shows(tree: &FragmentTree, word: &str) -> bool {
    tree.iter().any(|fragment| match &fragment.kind {
        FragmentKind::Text(run) => run.text.contains(word),
        _ => false,
    })
}

/// A frame with nothing said about its size is the default object size, three
/// hundred by a hundred and fifty, inside the user-agent sheet's two-pixel
/// border — and its attributes are its `width` and `height`.
#[test]
fn a_frame_is_the_default_object_size_unless_its_attributes_say_otherwise() {
    assert_eq!(size_of("<iframe></iframe>", "iframe"), (304.0, 154.0));
    assert_eq!(
        size_of("<iframe width=560 height=315></iframe>", "iframe"),
        (564.0, 319.0)
    );
    assert_eq!(
        size_of("<iframe width=560></iframe>", "iframe"),
        (564.0, 154.0),
        "no ratio to take the other side from, so the default's"
    );
    for frameborder in ["0", "no"] {
        assert_eq!(
            size_of(
                &format!("<iframe frameborder={frameborder}></iframe>"),
                "iframe"
            ),
            (300.0, 150.0),
            "{frameborder}"
        );
    }
    assert_eq!(
        size_of("<iframe frameborder=-1></iframe>", "iframe"),
        (304.0, 154.0)
    );
}

/// A video is a box whether or not anything plays in it, and what is written
/// inside it is for a browser that cannot play one.
#[test]
fn a_video_is_a_box_and_its_fallback_is_not_shown() {
    let (tree, boxes) = laid_out(
        "<body style='margin: 0'><video>Your browser does not support video</video>",
        800.0,
    );
    let video = rect_of(&tree, &boxes, "video");
    assert_eq!((video.width, video.height), (300.0, 150.0));
    assert!(!shows(&tree, "support"));

    assert_eq!(
        size_of("<video width=250 height=159></video>", "video"),
        (250.0, 159.0)
    );
    assert_eq!(
        size_of("<video width=250></video>", "video"),
        (250.0, 150.0)
    );
}

/// A video's poster is its picture, and its size is the video's until a frame
/// of the video says otherwise.
#[test]
fn a_poster_is_what_a_video_shows() {
    let (tree, boxes) = laid_out_with_image(
        "<body style='margin: 0'><video poster=p.png></video>",
        800.0,
        picture(320, 240),
    );
    let video = rect_of(&tree, &boxes, "video");
    assert_eq!((video.width, video.height), (320.0, 240.0));
    let poster = image_rect(&tree);
    assert_eq!((poster.width, poster.height), (320.0, 240.0));
}

/// A canvas is its bitmap's size, three hundred by a hundred and fifty unless
/// the attributes say otherwise, and it keeps the bitmap's shape when CSS
/// sizes it along one side. The attributes are not CSS: a `width` that does
/// not parse is the default, not nothing.
#[test]
fn a_canvas_is_its_bitmaps_size() {
    assert_eq!(
        size_of("<canvas width=100></canvas>", "canvas"),
        (100.0, 150.0)
    );
    assert_eq!(
        size_of("<canvas style='width: 200px'></canvas>", "canvas"),
        (200.0, 100.0)
    );
    assert_eq!(
        size_of("<canvas width=abc height=40></canvas>", "canvas"),
        (300.0, 40.0)
    );
    let (tree, _) = laid_out("<canvas>no canvas here</canvas>", 800.0);
    assert!(!shows(&tree, "canvas"));
}

/// An `object` whose resource is no picture shows what is inside it, which is
/// there for exactly that; one whose resource is a picture shows the picture
/// and nothing else.
#[test]
fn an_object_is_its_picture_or_its_fallback() {
    for html in [
        "<object data=x.swf><p>fallback</p></object>",
        "<object><p>fallback</p></object>",
    ] {
        let (tree, _) = laid_out(html, 800.0);
        assert!(shows(&tree, "fallback"), "{html}");
    }

    let (tree, boxes) = laid_out_with_image(
        "<body style='margin: 0'><object data=a.png><p>fallback</p></object>",
        800.0,
        picture(64, 32),
    );
    assert!(!shows(&tree, "fallback"));
    let object = rect_of(&tree, &boxes, "object");
    assert_eq!((object.width, object.height), (64.0, 32.0));
}

/// An `embed` that names something is a box of the default object size, and
/// one that names nothing is nothing.
#[test]
fn an_embed_is_a_box_when_it_names_something() {
    assert_eq!(size_of("<embed src=x.swf>", "embed"), (300.0, 150.0));
    assert_eq!(
        size_of("<embed src=x.swf width=200 height=100>", "embed"),
        (200.0, 100.0)
    );
    let (tree, boxes) = laid_out("<p><embed style='border: 5px solid'>", 800.0);
    assert!(
        boxes_of(&tree, &boxes, "embed")
            .iter()
            .all(|embed| embed.rect.width <= 10.0),
        "no content, so no room for any"
    );
}

/// An `audio` with controls takes the room the references' controls take, and
/// is no taller when it is made wider: the controls are one line high whatever
/// their width. Without controls it is not shown, whatever a page says.
#[test]
fn audio_is_as_big_as_its_controls_or_not_there() {
    assert_eq!(size_of("<audio controls></audio>", "audio"), (300.0, 54.0));
    assert_eq!(
        size_of("<audio controls style='width: 600px'></audio>", "audio"),
        (600.0, 54.0)
    );
    for html in [
        "<audio>no audio here</audio>x",
        "<style>audio { display: block }</style><audio></audio>x",
    ] {
        let (tree, boxes) = laid_out(html, 800.0);
        assert!(boxes_of(&tree, &boxes, "audio").is_empty(), "{html}");
        assert!(!shows(&tree, "audio"), "{html}");
    }
}

/// `width="40"` on a four-by-two picture is a width of forty, and the height
/// follows the picture's ratio rather than staying two.
#[test]
fn a_width_attribute_keeps_the_pictures_ratio() {
    let (tree, _) = laid_out_with_image("<img src=a.png width=40>", 800.0, picture(4, 2));
    let rect = image_rect(&tree);
    assert_eq!((rect.width, rect.height), (40.0, 20.0));
}

/// The markup every WordPress site writes: the attributes give the picture's
/// size, and the theme lets it shrink to its column with `height: auto`. The
/// attribute is a hint, beneath every rule a page writes, so `auto` wins and
/// the picture keeps its shape in a column narrower than itself.
#[test]
fn a_stylesheets_height_auto_beats_the_height_attribute() {
    let (tree, _) = laid_out_with_image(
        "<body style='margin: 0'><div style='width: 400px'>\
         <img src=a.png width=800 height=400 style='max-width: 100%; height: auto'>",
        800.0,
        picture(800, 400),
    );
    let rect = image_rect(&tree);
    assert_eq!((rect.width, rect.height), (400.0, 200.0));

    let (tree, _) = laid_out_with_image(
        "<style>img { width: 80px }</style><img src=a.png width=40>",
        800.0,
        picture(4, 2),
    );
    let rect = image_rect(&tree);
    assert_eq!((rect.width, rect.height), (80.0, 40.0));
}

/// `align=left` floats a picture, and the text runs down its right side
/// beyond the room `hspace` keeps; `vspace` keeps room above it, and `border`
/// draws a line round it.
#[test]
fn an_aligned_picture_floats_with_room_round_it() {
    let (tree, boxes) = laid_out_with_image(
        "<body style='margin: 0'>\
         <p style='margin: 0'><img src=a.png align=left hspace=10 vspace=5 border=2>beside",
        800.0,
        picture(100, 50),
    );
    let img = rect_of(&tree, &boxes, "img");
    assert_eq!((img.x, img.y), (10.0, 5.0));
    assert_eq!((img.width, img.height), (104.0, 54.0));
    let text = tree
        .iter()
        .find(|fragment| matches!(fragment.kind, FragmentKind::Text(_)))
        .expect("the text")
        .rect;
    assert_eq!(text.x, img.right() + 10.0);
    assert!(text.y < img.bottom(), "beside the picture, not below it");
}

/// An image button is its picture, with an `img`'s attributes; one with no
/// picture to show is its alternative text, which its `width` does not size.
#[test]
fn an_image_button_is_its_picture_or_its_text() {
    let (tree, boxes) = laid_out_with_image(
        "<body style='margin: 0'><input type=image src=a.png alt=Go \
         border=3 hspace=4 vspace=5 width=40 style='display: block'>",
        800.0,
        picture(320, 240),
    );
    let button = rect_of(&tree, &boxes, "input");
    assert_eq!((button.x, button.y), (4.0, 5.0));
    assert_eq!((button.width, button.height), (46.0, 36.0));
    assert!(!shows(&tree, "Go"));

    let (tree, boxes) = laid_out("<input type=image alt=Go width=400>", 800.0);
    assert!(shows(&tree, "Go"));
    assert!(rect_of(&tree, &boxes, "input").width < 100.0);
}
