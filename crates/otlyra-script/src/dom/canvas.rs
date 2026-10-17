//! What `canvas.js` calls: one native per element for every 2D context
//! operation, drawing into a bitmap the page's state keeps by element.
//!
//! The bitmaps are drawn here, on the thread the page's script runs on, and
//! the page is handed a copy of each one a turn changed — see
//! [`PageState::take_canvas_frames`](super::PageState::take_canvas_frames).

use otlyra_dom::{Document, NodeId};
use otlyra_gfx::canvas2d::{Canvas2D, CanvasFont, CanvasPaint, Source};

/// Run `op` on `node`'s bitmap, made at the size its attributes give it the
/// first time it is asked for. What it answers — a colour, a transform,
/// pixels, measurements — as numbers, and `None` for nothing.
pub(crate) fn run(
    document: &Document,
    node: NodeId,
    op: &str,
    numbers: &[f64],
    text: Option<&str>,
    source: Option<NodeId>,
) -> Option<Vec<f64>> {
    // Parsing a colour needs no bitmap.
    if op == "parseColor" {
        return parse_color(text?).map(|rgba| rgba.iter().map(|&c| f64::from(c)).collect());
    }
    let size = canvas_size(document, node);
    // A source drawn from: another canvas's bitmap, or an image's pixels.
    let image =
        source.and_then(|source| document.bitmap(source).map(|bitmap| (source, Some(bitmap))));
    let other_canvas = source.filter(|source| *source != node && image.is_none());
    let other = other_canvas.and_then(|source| {
        super::with_state_value(|state| {
            state.canvases.get_mut(&source).map(|canvas| {
                let (width, height) = canvas.size();
                (width, height, canvas.pixels())
            })
        })
        .flatten()
    });

    super::with_state_value(|state| {
        let canvas = state
            .canvases
            .entry(node)
            .or_insert_with(|| Canvas2D::new(size.0, size.1));
        let answer = apply(
            canvas,
            op,
            numbers,
            text,
            size,
            image.as_ref(),
            other.as_ref(),
        );
        if changes_pixels(op) && !state.dirty_canvases.contains(&node) {
            state.dirty_canvases.push(node);
        }
        answer
    })
    .flatten()
}

/// The PNG of `node`'s bitmap, base64, for `toDataURL`.
pub(crate) fn png(document: &Document, node: NodeId) -> Option<String> {
    let size = canvas_size(document, node);
    super::with_state_value(|state| {
        let canvas = state
            .canvases
            .entry(node)
            .or_insert_with(|| Canvas2D::new(size.0, size.1));
        canvas.to_png().map(|bytes| base64(&bytes))
    })
    .flatten()
}

/// Whether an operation can change what the bitmap shows.
fn changes_pixels(op: &str) -> bool {
    matches!(
        op,
        "fill"
            | "stroke"
            | "fillPath"
            | "strokePath"
            | "fillRect"
            | "strokeRect"
            | "clearRect"
            | "fillText"
            | "strokeText"
            | "drawImage"
            | "putImageData"
            | "resize"
            | "reset"
    )
}

/// The size a `<canvas>` element's attributes give its bitmap (HTML
/// §4.12.5): 300 by 150 where one is missing or not a number.
fn canvas_size(document: &Document, node: NodeId) -> (u32, u32) {
    let read = |name: &str, fallback: u32| {
        document
            .attr(node, name)
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(fallback)
    };
    (read("width", 300), read("height", 150))
}

fn f32s(numbers: &[f64]) -> Vec<f32> {
    numbers.iter().map(|&value| value as f32).collect()
}

fn arg(numbers: &[f64], index: usize) -> f32 {
    numbers.get(index).map_or(f32::NAN, |&value| value as f32)
}

fn apply(
    canvas: &mut Canvas2D,
    op: &str,
    numbers: &[f64],
    text: Option<&str>,
    size: (u32, u32),
    image: Option<&(NodeId, Option<otlyra_dom::Bitmap>)>,
    other: Option<&(u32, u32, Vec<u8>)>,
) -> Option<Vec<f64>> {
    let n = |index: usize| arg(numbers, index);
    let six = || [n(0), n(1), n(2), n(3), n(4), n(5)];
    let yes = || Some(vec![1.0]);
    match op {
        "resize" => canvas.resize(size.0, size.1),
        "reset" => canvas.reset(),
        "save" => canvas.save(),
        "restore" => canvas.restore(),
        "transform" => canvas.transform(six()),
        "setTransform" => canvas.set_transform(six()),
        "getTransform" => {
            return Some(
                canvas
                    .get_transform()
                    .iter()
                    .map(|&v| f64::from(v))
                    .collect(),
            );
        }
        "fillStyle" | "strokeStyle" => {
            let rgba = parse_color(text?)?;
            let paint = CanvasPaint::Color(rgba);
            if op == "fillStyle" {
                canvas.set_fill(paint);
            } else {
                canvas.set_stroke(paint);
            }
            return Some(rgba.iter().map(|&c| f64::from(c)).collect());
        }
        "fillStyleGradient" | "strokeStyleGradient" => {
            let values = f32s(numbers);
            let kind = *values.first()?;
            let (points, rest) = if kind == 0.0 {
                (values.get(1..5)?, values.get(5..)?)
            } else {
                (values.get(1..7)?, values.get(7..)?)
            };
            let stops = rest
                .as_chunks::<5>()
                .0
                .iter()
                .map(|stop| (stop[0], [stop[1], stop[2], stop[3], stop[4]]))
                .collect();
            let paint = if kind == 0.0 {
                CanvasPaint::Linear {
                    points: points.try_into().ok()?,
                    stops,
                }
            } else {
                CanvasPaint::Radial {
                    circles: points.try_into().ok()?,
                    stops,
                }
            };
            if op == "fillStyleGradient" {
                canvas.set_fill(paint);
            } else {
                canvas.set_stroke(paint);
            }
        }
        "lineWidth" => canvas.set_line_width(n(0)),
        "miterLimit" => canvas.set_miter_limit(n(0)),
        "globalAlpha" => canvas.set_global_alpha(n(0)),
        "lineDashOffset" => canvas.set_line_dash_offset(n(0)),
        "setLineDash" => canvas.set_line_dash(&f32s(numbers)),
        "lineCap" => {
            canvas.set_line_cap(text?);
        }
        "lineJoin" => {
            canvas.set_line_join(text?);
        }
        "textAlign" => {
            canvas.set_text_align(text?);
        }
        "textBaseline" => {
            canvas.set_text_baseline(text?);
        }
        "globalCompositeOperation" => {
            return canvas.set_composite(text?).then(yes).flatten();
        }
        "imageSmoothingEnabled" => canvas.set_image_smoothing(n(0) != 0.0),
        "font" => canvas.set_font(CanvasFont {
            families: text?.split('\n').map(str::to_owned).collect(),
            size: n(0),
            weight: n(1) as i32,
            italic: n(2) != 0.0,
        }),
        "beginPath" => canvas.begin_path(),
        "closePath" => canvas.close_path(),
        "moveTo" => canvas.move_to(n(0), n(1)),
        "lineTo" => canvas.line_to(n(0), n(1)),
        "quadraticCurveTo" => canvas.quadratic_curve_to(n(0), n(1), n(2), n(3)),
        "bezierCurveTo" => canvas.bezier_curve_to(six()),
        "arc" => canvas.arc(n(0), n(1), n(2), n(3), n(4), n(5) != 0.0),
        "arcTo" => canvas.arc_to(n(0), n(1), n(2), n(3), n(4)),
        "ellipse" => canvas.ellipse(n(0), n(1), n(2), n(3), n(4), n(5), n(6), n(7) != 0.0),
        "rect" => canvas.rect(n(0), n(1), n(2), n(3)),
        "roundRect" => canvas.round_rect(n(0), n(1), n(2), n(3), [n(4), n(5), n(6), n(7)]),
        "fill" => canvas.fill(n(0) != 0.0),
        "stroke" => canvas.stroke(),
        "clip" => canvas.clip(n(0) != 0.0),
        "isPointInPath" => {
            let inside = canvas.is_point_in_path(n(0), n(1), n(2) != 0.0);
            return Some(vec![if inside { 1.0 } else { 0.0 }]);
        }
        "fillPath" | "strokePath" | "clipPath" => {
            let evenodd = n(0) != 0.0;
            let path = canvas.with_path(|canvas| replay(canvas, &numbers[1..], text));
            match op {
                "fillPath" => canvas.fill_path(&path, evenodd),
                "strokePath" => canvas.stroke_path(&path),
                _ => canvas.clip_path(&path, evenodd),
            }
        }
        "isPointInPathOf" => {
            let (x, y, evenodd) = (n(0), n(1), n(2) != 0.0);
            let path = canvas.with_path(|canvas| replay(canvas, &numbers[3..], text));
            let inside = path.contains(x, y, evenodd);
            return Some(vec![if inside { 1.0 } else { 0.0 }]);
        }
        "fillRect" => canvas.fill_rect(n(0), n(1), n(2), n(3)),
        "strokeRect" => canvas.stroke_rect(n(0), n(1), n(2), n(3)),
        "clearRect" => canvas.clear_rect(n(0), n(1), n(2), n(3)),
        "fillText" | "strokeText" => {
            let max = (numbers.len() > 2).then(|| n(2));
            canvas.draw_text(text?, n(0), n(1), max, op == "strokeText");
        }
        "measureText" => {
            let m = canvas.measure_text(text?);
            return Some(
                [
                    m.width,
                    m.left,
                    m.right,
                    m.ascent,
                    m.descent,
                    m.font_ascent,
                    m.font_descent,
                ]
                .iter()
                .map(|&v| f64::from(v))
                .collect(),
            );
        }
        "drawImage" => {
            let from = [n(0), n(1), n(2), n(3)];
            let to = [n(4), n(5), n(6), n(7)];
            if let Some((_, Some(bitmap))) = image {
                canvas.draw_image(
                    Source {
                        width: bitmap.width,
                        height: bitmap.height,
                        pixels: bitmap.pixels.as_ref().as_ref(),
                        premultiplied: bitmap.premultiplied,
                    },
                    from,
                    to,
                );
            } else if let Some((width, height, pixels)) = other {
                canvas.draw_image(
                    Source {
                        width: *width,
                        height: *height,
                        pixels,
                        premultiplied: true,
                    },
                    from,
                    to,
                );
            } else if image.is_none() && other.is_none() {
                // A canvas drawn onto itself: what it shows now.
                let (width, height) = canvas.size();
                let pixels = canvas.pixels();
                canvas.draw_image(
                    Source {
                        width: width.max(1),
                        height: height.max(1),
                        pixels: &pixels,
                        premultiplied: true,
                    },
                    from,
                    to,
                );
            }
        }
        "getImageData" => {
            let (w, h) = (n(2).max(0.0) as u32, n(3).max(0.0) as u32);
            let pixels = canvas.get_image_data(n(0) as i32, n(1) as i32, w, h);
            return Some(pixels.into_iter().map(f64::from).collect());
        }
        "putImageData" => {
            let (w, h) = (n(0).max(0.0) as u32, n(1).max(0.0) as u32);
            let pixels: Vec<u8> = numbers
                .get(4..)?
                .iter()
                .map(|&value| value.clamp(0.0, 255.0) as u8)
                .collect();
            canvas.put_image_data(&pixels, w, h, n(2) as i32, n(3) as i32);
        }
        _ => return None,
    }
    None
}

/// Replay a `Path2D`'s commands, as `canvas.js` encodes them: a code, how
/// many arguments, the arguments — and its SVG path data, in order, in
/// `svg`.
fn replay(canvas: &mut Canvas2D, numbers: &[f64], svg: Option<&str>) {
    let mut svg = svg.unwrap_or_default().split('\0');
    let mut at = 0;
    while at + 1 < numbers.len() {
        let code = numbers[at] as u32;
        let count = numbers[at + 1] as usize;
        let args = numbers.get(at + 2..at + 2 + count).unwrap_or_default();
        at += 2 + count;
        let a = |index: usize| arg(args, index);
        match code {
            0 => canvas.close_path(),
            1 => canvas.move_to(a(0), a(1)),
            2 => canvas.line_to(a(0), a(1)),
            3 => canvas.bezier_curve_to([a(0), a(1), a(2), a(3), a(4), a(5)]),
            4 => canvas.quadratic_curve_to(a(0), a(1), a(2), a(3)),
            5 => canvas.arc(a(0), a(1), a(2), a(3), a(4), a(5) != 0.0),
            6 => canvas.arc_to(a(0), a(1), a(2), a(3), a(4)),
            7 => canvas.ellipse(a(0), a(1), a(2), a(3), a(4), a(5), a(6), a(7) != 0.0),
            8 => canvas.rect(a(0), a(1), a(2), a(3)),
            9 => canvas.round_rect(a(0), a(1), a(2), a(3), [a(4), a(5), a(6), a(7)]),
            10 => {
                if let Some(data) = svg.next() {
                    canvas.svg_path(data);
                }
            }
            _ => {}
        }
    }
}

/// A CSS colour as straight RGBA in 0..=1: hex, `rgb()`/`rgba()`,
/// `hsl()`/`hsla()`, a named colour, `transparent`. `None` for anything else,
/// which a canvas ignores.
pub(crate) fn parse_color(text: &str) -> Option<[f32; 4]> {
    let text = text.trim().to_ascii_lowercase();
    if text == "transparent" {
        return Some([0.0; 4]);
    }
    if let Some(hex) = text.strip_prefix('#') {
        let (r, g, b, a) = cssparser::color::parse_hash_color(hex.as_bytes()).ok()?;
        return Some([
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            a,
        ]);
    }
    if let Some((name, rest)) = text.split_once('(') {
        let inner = rest.strip_suffix(')')?;
        let parts: Vec<&str> = inner
            .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() < 3 {
            return None;
        }
        let alpha = match parts.get(3) {
            Some(part) => match part.strip_suffix('%') {
                Some(percent) => percent.parse::<f32>().ok()? / 100.0,
                None => part.parse::<f32>().ok()?,
            },
            None => 1.0,
        }
        .clamp(0.0, 1.0);
        return match name.trim() {
            "rgb" | "rgba" => {
                let channel = |part: &str| -> Option<f32> {
                    Some(match part.strip_suffix('%') {
                        Some(percent) => percent.parse::<f32>().ok()? / 100.0,
                        None => part.parse::<f32>().ok()? / 255.0,
                    })
                };
                Some([
                    channel(parts[0])?.clamp(0.0, 1.0),
                    channel(parts[1])?.clamp(0.0, 1.0),
                    channel(parts[2])?.clamp(0.0, 1.0),
                    alpha,
                ])
            }
            "hsl" | "hsla" => {
                let hue = parts[0].trim_end_matches("deg").parse::<f32>().ok()?;
                let percent = |part: &str| -> Option<f32> {
                    Some(part.trim_end_matches('%').parse::<f32>().ok()? / 100.0)
                };
                let (r, g, b) = hsl(hue, percent(parts[1])?, percent(parts[2])?);
                Some([r, g, b, alpha])
            }
            _ => None,
        };
    }
    let (r, g, b) = cssparser::color::parse_named_color(&text).ok()?;
    Some([
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        1.0,
    ])
}

fn hsl(hue: f32, saturation: f32, lightness: f32) -> (f32, f32, f32) {
    let (s, l) = (saturation.clamp(0.0, 1.0), lightness.clamp(0.0, 1.0));
    let h = hue.rem_euclid(360.0) / 30.0;
    let a = s * l.min(1.0 - l);
    let f = |n: f32| {
        let k = (n + h) % 12.0;
        l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    (f(0.0), f(8.0), f(4.0))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_as_a_canvas_reads_them() {
        assert_eq!(parse_color("#f00"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(
            parse_color("rgba(0, 255, 0, 0.5)"),
            Some([0.0, 1.0, 0.0, 0.5])
        );
        assert_eq!(
            parse_color("rgb(0 0 255 / 50%)"),
            Some([0.0, 0.0, 1.0, 0.5])
        );
        assert_eq!(parse_color("White"), Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(parse_color("transparent"), Some([0.0; 4]));
        let red = parse_color("hsl(0, 100%, 50%)").unwrap();
        assert!((red[0] - 1.0).abs() < 1e-5 && red[1].abs() < 1e-5);
        assert_eq!(parse_color("nonsense"), None);
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"M"), "TQ==");
    }
}
