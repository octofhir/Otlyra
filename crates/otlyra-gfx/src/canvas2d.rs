//! A `<canvas>` element's 2D bitmap: what `getContext('2d')` draws into.
//!
//! Drawn on the CPU, into pixels of its own, and handed to the page as a
//! picture: a canvas is a replaced element whose content is this bitmap, and
//! the page draws it the way it draws an `<img>`.
//!
//! Paths are kept in the bitmap's own pixels: each point is put through the
//! transform current when it was added, which is what the HTML specification
//! says a path is (§4.12.5.1.7). So a fill draws with no transform at all, and
//! a stroke — whose width is in the user's units — draws the path mapped back
//! through the transform current when it is stroked.

use skia_safe as sk;

/// What a fill or a stroke paints with.
#[derive(Clone, Debug, PartialEq)]
pub enum CanvasPaint {
    /// One colour, straight (not premultiplied) RGBA in 0..=1.
    Color([f32; 4]),
    /// `createLinearGradient`: from one point to another.
    Linear {
        /// The start and end points.
        points: [f32; 4],
        /// Each stop's offset in 0..=1 and its colour.
        stops: Vec<(f32, [f32; 4])>,
    },
    /// `createRadialGradient`: between two circles, each `[x, y, r]`.
    Radial {
        /// The start and end circles.
        circles: [f32; 6],
        /// Each stop's offset in 0..=1 and its colour.
        stops: Vec<(f32, [f32; 4])>,
    },
}

/// The font text is drawn in: what the `font` attribute parses to.
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasFont {
    /// The families, in order of preference, generic names included.
    pub families: Vec<String>,
    /// The size in pixels.
    pub size: f32,
    /// The weight, 1..=1000.
    pub weight: i32,
    /// Whether it is italic.
    pub italic: bool,
}

impl Default for CanvasFont {
    /// `10px sans-serif`, the initial value.
    fn default() -> Self {
        Self {
            families: vec!["sans-serif".to_owned()],
            size: 10.0,
            weight: 400,
            italic: false,
        }
    }
}

/// A bitmap drawn somewhere else, to be drawn into this one.
#[derive(Clone, Copy, Debug)]
pub struct Source<'a> {
    /// Its width in pixels.
    pub width: u32,
    /// Its height in pixels.
    pub height: u32,
    /// RGBA, four bytes a pixel, row after row.
    pub pixels: &'a [u8],
    /// Whether the colour is already multiplied by the alpha.
    pub premultiplied: bool,
}

/// What `measureText` answers, in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextMetrics {
    /// The advance.
    pub width: f32,
    /// How far the ink reaches left of the origin.
    pub left: f32,
    /// How far the ink reaches right of the origin.
    pub right: f32,
    /// How far the ink reaches above the baseline.
    pub ascent: f32,
    /// How far the ink reaches below the baseline.
    pub descent: f32,
    /// How far the font reaches above the baseline.
    pub font_ascent: f32,
    /// How far the font reaches below the baseline.
    pub font_descent: f32,
}

#[derive(Clone)]
struct State {
    fill: CanvasPaint,
    stroke: CanvasPaint,
    line_width: f32,
    cap: sk::paint::Cap,
    join: sk::paint::Join,
    miter: f32,
    alpha: f32,
    blend: sk::BlendMode,
    font: CanvasFont,
    align: Align,
    baseline: Baseline,
    dash: Vec<f32>,
    dash_offset: f32,
    smoothing: bool,
    matrix: sk::Matrix,
}

impl Default for State {
    fn default() -> Self {
        Self {
            fill: CanvasPaint::Color([0.0, 0.0, 0.0, 1.0]),
            stroke: CanvasPaint::Color([0.0, 0.0, 0.0, 1.0]),
            line_width: 1.0,
            cap: sk::paint::Cap::Butt,
            join: sk::paint::Join::Miter,
            miter: 10.0,
            alpha: 1.0,
            blend: sk::BlendMode::SrcOver,
            font: CanvasFont::default(),
            align: Align::Start,
            baseline: Baseline::Alphabetic,
            dash: Vec::new(),
            dash_offset: 0.0,
            smoothing: true,
            matrix: sk::Matrix::new_identity(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Start,
    End,
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Baseline {
    Top,
    Hanging,
    Middle,
    Alphabetic,
    Ideographic,
    Bottom,
}

/// The largest side a bitmap may have, and the most pixels it may hold: past
/// either, the canvas stays one transparent pixel, as a browser that cannot
/// allocate the bitmap does.
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

/// A path made apart from the current one, in the bitmap's pixels.
#[derive(Clone, Debug)]
pub struct CanvasPath(sk::Path);

impl CanvasPath {
    /// Whether a point of the bitmap is inside it, by the rule named.
    pub fn contains(&self, x: f32, y: f32, evenodd: bool) -> bool {
        with_rule(&self.0, evenodd).contains((x, y))
    }
}

/// One `<canvas>` element's bitmap and its 2D context's state.
pub struct Canvas2D {
    surface: sk::Surface,
    width: u32,
    height: u32,
    state: State,
    stack: Vec<State>,
    path: sk::PathBuilder,
    font_mgr: Option<sk::FontMgr>,
}

impl std::fmt::Debug for Canvas2D {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canvas2D")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

fn surface(width: u32, height: u32) -> sk::Surface {
    let fits = width <= MAX_SIDE
        && height <= MAX_SIDE
        && u64::from(width) * u64::from(height) <= MAX_PIXELS;
    let (width, height) = if fits {
        (width.max(1), height.max(1))
    } else {
        (1, 1)
    };
    let info = sk::ImageInfo::new(
        (width as i32, height as i32),
        sk::ColorType::RGBA8888,
        sk::AlphaType::Premul,
        None,
    );
    sk::surfaces::raster(&info, None, None).expect("a raster surface of a size checked above")
}

impl Canvas2D {
    /// A transparent bitmap `width` by `height` pixels.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            surface: surface(width, height),
            width,
            height,
            state: State::default(),
            stack: Vec::new(),
            path: sk::PathBuilder::new(),
            font_mgr: None,
        }
    }

    /// Its size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Set its size, which clears it and its context's state (HTML
    /// §4.12.5.1.1): even to the size it already has.
    pub fn resize(&mut self, width: u32, height: u32) {
        *self = Self {
            font_mgr: self.font_mgr.take(),
            ..Self::new(width, height)
        };
    }

    // The state.

    /// `save()`.
    pub fn save(&mut self) {
        self.stack.push(self.state.clone());
        self.surface.canvas().save();
    }

    /// `restore()`: nothing when nothing was saved.
    pub fn restore(&mut self) {
        if let Some(state) = self.stack.pop() {
            self.state = state;
            self.surface.canvas().restore();
        }
    }

    /// `reset()`: back to a cleared bitmap and the initial state.
    pub fn reset(&mut self) {
        self.resize(self.width, self.height);
    }

    /// `fillStyle`.
    pub fn set_fill(&mut self, paint: CanvasPaint) {
        self.state.fill = paint;
    }

    /// `strokeStyle`.
    pub fn set_stroke(&mut self, paint: CanvasPaint) {
        self.state.stroke = paint;
    }

    /// `lineWidth`: a value that is not positive and finite is ignored.
    pub fn set_line_width(&mut self, width: f32) {
        if width.is_finite() && width > 0.0 {
            self.state.line_width = width;
        }
    }

    /// `lineCap`; whether the value is one.
    pub fn set_line_cap(&mut self, cap: &str) -> bool {
        self.state.cap = match cap {
            "butt" => sk::paint::Cap::Butt,
            "round" => sk::paint::Cap::Round,
            "square" => sk::paint::Cap::Square,
            _ => return false,
        };
        true
    }

    /// `lineJoin`; whether the value is one.
    pub fn set_line_join(&mut self, join: &str) -> bool {
        self.state.join = match join {
            "miter" => sk::paint::Join::Miter,
            "round" => sk::paint::Join::Round,
            "bevel" => sk::paint::Join::Bevel,
            _ => return false,
        };
        true
    }

    /// `miterLimit`: a value that is not positive and finite is ignored.
    pub fn set_miter_limit(&mut self, limit: f32) {
        if limit.is_finite() && limit > 0.0 {
            self.state.miter = limit;
        }
    }

    /// `globalAlpha`: outside 0..=1 is ignored.
    pub fn set_global_alpha(&mut self, alpha: f32) {
        if (0.0..=1.0).contains(&alpha) {
            self.state.alpha = alpha;
        }
    }

    /// `globalCompositeOperation`; whether the value is one.
    pub fn set_composite(&mut self, operation: &str) -> bool {
        use sk::BlendMode as B;
        self.state.blend = match operation {
            "source-over" => B::SrcOver,
            "source-in" => B::SrcIn,
            "source-out" => B::SrcOut,
            "source-atop" => B::SrcATop,
            "destination-over" => B::DstOver,
            "destination-in" => B::DstIn,
            "destination-out" => B::DstOut,
            "destination-atop" => B::DstATop,
            "lighter" => B::Plus,
            "copy" => B::Src,
            "xor" => B::Xor,
            "multiply" => B::Multiply,
            "screen" => B::Screen,
            "overlay" => B::Overlay,
            "darken" => B::Darken,
            "lighten" => B::Lighten,
            "color-dodge" => B::ColorDodge,
            "color-burn" => B::ColorBurn,
            "hard-light" => B::HardLight,
            "soft-light" => B::SoftLight,
            "difference" => B::Difference,
            "exclusion" => B::Exclusion,
            "hue" => B::Hue,
            "saturation" => B::Saturation,
            "color" => B::Color,
            "luminosity" => B::Luminosity,
            _ => return false,
        };
        true
    }

    /// `font`, parsed.
    pub fn set_font(&mut self, font: CanvasFont) {
        self.state.font = font;
    }

    /// `textAlign`; whether the value is one.
    pub fn set_text_align(&mut self, align: &str) -> bool {
        self.state.align = match align {
            "start" => Align::Start,
            "end" => Align::End,
            "left" => Align::Left,
            "right" => Align::Right,
            "center" => Align::Center,
            _ => return false,
        };
        true
    }

    /// `textBaseline`; whether the value is one.
    pub fn set_text_baseline(&mut self, baseline: &str) -> bool {
        self.state.baseline = match baseline {
            "top" => Baseline::Top,
            "hanging" => Baseline::Hanging,
            "middle" => Baseline::Middle,
            "alphabetic" => Baseline::Alphabetic,
            "ideographic" => Baseline::Ideographic,
            "bottom" => Baseline::Bottom,
            _ => return false,
        };
        true
    }

    /// `setLineDash`: a list with a value that is not finite and
    /// non-negative is ignored, and an odd one is repeated to be even.
    pub fn set_line_dash(&mut self, segments: &[f32]) {
        if segments
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return;
        }
        let mut dash = segments.to_vec();
        if dash.len() % 2 == 1 {
            dash.extend_from_slice(segments);
        }
        self.state.dash = dash;
    }

    /// `lineDashOffset`.
    pub fn set_line_dash_offset(&mut self, offset: f32) {
        if offset.is_finite() {
            self.state.dash_offset = offset;
        }
    }

    /// `imageSmoothingEnabled`.
    pub fn set_image_smoothing(&mut self, smoothing: bool) {
        self.state.smoothing = smoothing;
    }

    // The transform.

    /// `transform(a, b, c, d, e, f)`: multiplied onto the current one.
    pub fn transform(&mut self, m: [f32; 6]) {
        if m.iter().all(|value| value.is_finite()) {
            self.state.matrix = sk::Matrix::concat(&self.state.matrix, &matrix(m));
        }
    }

    /// `setTransform(a, b, c, d, e, f)`.
    pub fn set_transform(&mut self, m: [f32; 6]) {
        if m.iter().all(|value| value.is_finite()) {
            self.state.matrix = matrix(m);
        }
    }

    /// `getTransform()`, as `[a, b, c, d, e, f]`.
    pub fn get_transform(&self) -> [f32; 6] {
        let m = &self.state.matrix;
        [
            m.scale_x(),
            m.skew_y(),
            m.skew_x(),
            m.scale_y(),
            m.translate_x(),
            m.translate_y(),
        ]
    }

    // The path.

    /// `beginPath()`.
    pub fn begin_path(&mut self) {
        self.path.reset();
    }

    /// `closePath()`.
    pub fn close_path(&mut self) {
        if !self.path.is_empty() {
            self.path.close();
        }
    }

    /// `moveTo(x, y)`.
    pub fn move_to(&mut self, x: f32, y: f32) {
        if finite(&[x, y]) {
            let point = self.state.matrix.map_point((x, y));
            self.path.move_to(point);
        }
    }

    /// `lineTo(x, y)`.
    pub fn line_to(&mut self, x: f32, y: f32) {
        if !finite(&[x, y]) {
            return;
        }
        let point = self.state.matrix.map_point((x, y));
        if self.path.is_empty() {
            self.path.move_to(point);
        } else {
            self.path.line_to(point);
        }
    }

    /// `quadraticCurveTo(cx, cy, x, y)`.
    pub fn quadratic_curve_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        if !finite(&[cx, cy, x, y]) {
            return;
        }
        self.ensure_subpath(cx, cy);
        let m = self.state.matrix;
        self.path
            .quad_to(m.map_point((cx, cy)), m.map_point((x, y)));
    }

    /// `bezierCurveTo(c1x, c1y, c2x, c2y, x, y)`.
    pub fn bezier_curve_to(&mut self, c: [f32; 6]) {
        if !finite(&c) {
            return;
        }
        self.ensure_subpath(c[0], c[1]);
        let m = self.state.matrix;
        self.path.cubic_to(
            m.map_point((c[0], c[1])),
            m.map_point((c[2], c[3])),
            m.map_point((c[4], c[5])),
        );
    }

    /// `arc(x, y, radius, start, end, counterclockwise)`.
    pub fn arc(&mut self, x: f32, y: f32, radius: f32, start: f32, end: f32, ccw: bool) {
        self.ellipse(x, y, radius, radius, 0.0, start, end, ccw);
    }

    /// `ellipse(x, y, rx, ry, rotation, start, end, counterclockwise)`.
    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(
        &mut self,
        x: f32,
        y: f32,
        rx: f32,
        ry: f32,
        rotation: f32,
        start: f32,
        end: f32,
        ccw: bool,
    ) {
        if !finite(&[x, y, rx, ry, rotation, start, end]) || rx < 0.0 || ry < 0.0 {
            return;
        }
        let sweep = arc_sweep(start, end, ccw).to_degrees();
        let oval = sk::Rect::new(-rx, -ry, rx, ry);
        let mut arc = sk::PathBuilder::new();
        let mut from = start.to_degrees();
        // Skia draws a sweep of a whole turn as nothing; two halves are the
        // whole of it.
        let parts = if sweep.abs() >= 360.0 { 2 } else { 1 };
        let part = sweep / parts as f32;
        for index in 0..parts {
            arc.arc_to(oval, from, part, index == 0);
            from += part;
        }
        let mut placed = sk::Matrix::translate((x, y));
        placed.pre_rotate(rotation.to_degrees(), None);
        let m = sk::Matrix::concat(&self.state.matrix, &placed);
        self.path
            .add_path_with_transform(&arc.detach(), &m, sk::path::AddPathMode::Extend);
    }

    /// `arcTo(x1, y1, x2, y2, radius)`.
    pub fn arc_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, radius: f32) {
        if !finite(&[x1, y1, x2, y2, radius]) || radius < 0.0 {
            return;
        }
        self.ensure_subpath(x1, y1);
        let Some(inverse) = self.state.matrix.invert() else {
            return;
        };
        let Some(last) = self.path.get_last_pt() else {
            return;
        };
        let here = inverse.map_point(last);
        let mut arc = sk::PathBuilder::new();
        arc.move_to(here);
        arc.arc_to_tangent((x1, y1), (x2, y2), radius);
        self.path.add_path_with_transform(
            &arc.detach(),
            &self.state.matrix,
            sk::path::AddPathMode::Extend,
        );
    }

    /// `rect(x, y, w, h)`: a closed subpath, and a new one at its corner.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if !finite(&[x, y, w, h]) {
            return;
        }
        let mut rect = sk::PathBuilder::new();
        rect.move_to((x, y))
            .line_to((x + w, y))
            .line_to((x + w, y + h))
            .line_to((x, y + h))
            .close();
        self.path.add_path_with_transform(
            &rect.detach(),
            &self.state.matrix,
            sk::path::AddPathMode::Append,
        );
        self.move_to(x, y);
    }

    /// `roundRect(x, y, w, h, radii)`, with one radius per corner from the
    /// top left, clockwise.
    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radii: [f32; 4]) {
        if !finite(&[x, y, w, h]) || !finite(&radii) {
            return;
        }
        let rect = sk::Rect::new(x, y, x + w, y + h).sorted();
        let corners = radii.map(|radius| sk::Vector::new(radius.max(0.0), radius.max(0.0)));
        let rrect = sk::RRect::new_rect_radii(rect, &corners);
        let mut shape = sk::PathBuilder::new();
        shape.add_rrect(rrect, None, None);
        self.path.add_path_with_transform(
            &shape.detach(),
            &self.state.matrix,
            sk::path::AddPathMode::Append,
        );
        self.move_to(x, y);
    }

    /// A subpath to add to, starting at `(x, y)` if there is none.
    fn ensure_subpath(&mut self, x: f32, y: f32) {
        if self.path.is_empty() {
            self.move_to(x, y);
        }
    }

    /// `fill(rule)`, of the current path.
    pub fn fill(&mut self, evenodd: bool) {
        let path = self.current_path(evenodd);
        self.fill_device_path(&path);
    }

    /// `stroke()`, of the current path.
    pub fn stroke(&mut self) {
        let path = self.current_path(false);
        self.stroke_device_path(&path);
    }

    /// `clip(rule)`, to the current path.
    pub fn clip(&mut self, evenodd: bool) {
        let path = self.current_path(evenodd);
        let canvas = self.surface.canvas();
        canvas.reset_matrix();
        canvas.clip_path(&path, sk::ClipOp::Intersect, true);
    }

    /// `isPointInPath(x, y, rule)`: the point is in the bitmap's pixels.
    pub fn is_point_in_path(&self, x: f32, y: f32, evenodd: bool) -> bool {
        self.current_path(evenodd).contains((x, y))
    }

    /// A separate path, as `Path2D` records one: what `build` adds, put
    /// through the current transform, leaving the current path as it was.
    pub fn with_path(&mut self, build: impl FnOnce(&mut Self)) -> CanvasPath {
        let kept = std::mem::take(&mut self.path);
        build(self);
        let path = self.path.detach();
        self.path = kept;
        CanvasPath(path)
    }

    /// Add SVG path data (`new Path2D('M0 0 L…')`), through the current
    /// transform. Data that does not parse adds nothing.
    pub fn svg_path(&mut self, data: &str) {
        if let Some(path) = sk::utils::parse_path::from_svg(data) {
            self.path.add_path_with_transform(
                &path,
                &self.state.matrix,
                sk::path::AddPathMode::Append,
            );
        }
    }

    /// Fill a path [`Canvas2D::with_path`] made.
    pub fn fill_path(&mut self, path: &CanvasPath, evenodd: bool) {
        let path = with_rule(&path.0, evenodd);
        self.fill_device_path(&path);
    }

    /// Stroke a path [`Canvas2D::with_path`] made.
    pub fn stroke_path(&mut self, path: &CanvasPath) {
        self.stroke_device_path(&path.0);
    }

    /// Clip to a path [`Canvas2D::with_path`] made.
    pub fn clip_path(&mut self, path: &CanvasPath, evenodd: bool) {
        let path = with_rule(&path.0, evenodd);
        let canvas = self.surface.canvas();
        canvas.reset_matrix();
        canvas.clip_path(&path, sk::ClipOp::Intersect, true);
    }

    fn current_path(&self, evenodd: bool) -> sk::Path {
        with_rule(&self.path.snapshot(), evenodd)
    }

    fn fill_device_path(&mut self, path: &sk::Path) {
        let paint = self.paint(&self.state.fill.clone(), true);
        let canvas = self.surface.canvas();
        canvas.reset_matrix();
        canvas.draw_path(path, &paint);
    }

    fn stroke_device_path(&mut self, path: &sk::Path) {
        let Some(inverse) = self.state.matrix.invert() else {
            return;
        };
        let user = path.make_transform(&inverse);
        let mut paint = self.paint(&self.state.stroke.clone(), false);
        paint.set_style(sk::paint::Style::Stroke);
        paint.set_stroke_width(self.state.line_width);
        paint.set_stroke_cap(self.state.cap);
        paint.set_stroke_join(self.state.join);
        paint.set_stroke_miter(self.state.miter);
        if !self.state.dash.is_empty() && self.state.dash.iter().any(|value| *value > 0.0) {
            paint.set_path_effect(sk::PathEffect::dash(
                &self.state.dash,
                self.state.dash_offset,
            ));
        }
        let matrix = self.state.matrix;
        let canvas = self.surface.canvas();
        canvas.set_matrix(&matrix.into());
        canvas.draw_path(&user, &paint);
        canvas.reset_matrix();
    }

    /// A paint for `source` with the global alpha and composite. A fill is
    /// drawn in the bitmap's pixels and a stroke in the user's units, so a
    /// gradient, which is in the user's, is moved into the bitmap's for a
    /// fill.
    fn paint(&self, source: &CanvasPaint, device: bool) -> sk::Paint {
        let mut paint = sk::Paint::default();
        paint.set_anti_alias(true);
        paint.set_blend_mode(self.state.blend);
        let alpha = self.state.alpha;
        let local = device.then_some(self.state.matrix);
        match source {
            CanvasPaint::Color(rgba) => {
                paint.set_color4f(color4f(*rgba, alpha), None);
            }
            CanvasPaint::Linear { points, stops } => {
                let (colors, offsets) = gradient_stops(stops, alpha);
                let stops = sk::gradient::Colors::new(
                    &colors,
                    Some(&offsets[..]),
                    sk::TileMode::Clamp,
                    None,
                );
                let gradient =
                    sk::gradient::Gradient::new(stops, sk::gradient::Interpolation::default());
                let shader = sk::shaders::linear_gradient(
                    (
                        sk::Point::new(points[0], points[1]),
                        sk::Point::new(points[2], points[3]),
                    ),
                    &gradient,
                    local.as_ref(),
                );
                paint.set_shader(shader);
            }
            CanvasPaint::Radial { circles, stops } => {
                let (colors, offsets) = gradient_stops(stops, alpha);
                let stops = sk::gradient::Colors::new(
                    &colors,
                    Some(&offsets[..]),
                    sk::TileMode::Clamp,
                    None,
                );
                let gradient =
                    sk::gradient::Gradient::new(stops, sk::gradient::Interpolation::default());
                let shader = sk::shaders::two_point_conical_gradient(
                    (sk::Point::new(circles[0], circles[1]), circles[2]),
                    (sk::Point::new(circles[3], circles[4]), circles[5]),
                    &gradient,
                    local.as_ref(),
                );
                paint.set_shader(shader);
            }
        }
        paint
    }

    // Rectangles.

    /// `fillRect(x, y, w, h)`.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if let Some(path) = self.rect_path(x, y, w, h) {
            self.fill_device_path(&path);
        }
    }

    /// `strokeRect(x, y, w, h)`.
    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if let Some(path) = self.rect_path(x, y, w, h) {
            self.stroke_device_path(&path);
        }
    }

    /// `clearRect`: transparent black, through the transform and the clip.
    pub fn clear_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let Some(path) = self.rect_path(x, y, w, h) else {
            return;
        };
        let mut paint = sk::Paint::default();
        paint.set_blend_mode(sk::BlendMode::Clear);
        let canvas = self.surface.canvas();
        canvas.reset_matrix();
        canvas.draw_path(&path, &paint);
    }

    fn rect_path(&self, x: f32, y: f32, w: f32, h: f32) -> Option<sk::Path> {
        if !finite(&[x, y, w, h]) {
            return None;
        }
        let mut rect = sk::PathBuilder::new();
        rect.move_to((x, y))
            .line_to((x + w, y))
            .line_to((x + w, y + h))
            .line_to((x, y + h))
            .close();
        Some(rect.detach().make_transform(&self.state.matrix))
    }

    // Text.

    fn font(&mut self) -> sk::Font {
        let font = self.state.font.clone();
        let style = sk::FontStyle::new(
            sk::font_style::Weight::from(font.weight),
            sk::font_style::Width::NORMAL,
            if font.italic {
                sk::font_style::Slant::Italic
            } else {
                sk::font_style::Slant::Upright
            },
        );
        let manager = self.font_mgr.get_or_insert_with(sk::FontMgr::new);
        let typeface = font
            .families
            .iter()
            .find_map(|family| {
                let name = match family.as_str() {
                    "sans-serif" | "system-ui" | "-apple-system" | "ui-sans-serif" => "Helvetica",
                    "serif" | "ui-serif" => "Times",
                    "monospace" | "ui-monospace" => "Menlo",
                    "cursive" => "Apple Chancery",
                    "fantasy" => "Papyrus",
                    other => other,
                };
                manager.match_family_style(name, style)
            })
            .or_else(|| manager.legacy_make_typeface(None, style));
        match typeface {
            Some(typeface) => sk::Font::from_typeface(typeface, font.size),
            None => sk::Font::default(),
        }
    }

    /// `measureText(text)`.
    pub fn measure_text(&mut self, text: &str) -> TextMetrics {
        let font = self.font();
        let (width, bounds) = font.measure_str(text, None);
        let (_, metrics) = font.metrics();
        TextMetrics {
            width,
            left: -bounds.left,
            right: bounds.right,
            ascent: -bounds.top,
            descent: bounds.bottom,
            font_ascent: -metrics.ascent,
            font_descent: metrics.descent,
        }
    }

    /// `fillText` and `strokeText`.
    pub fn draw_text(&mut self, text: &str, x: f32, y: f32, max_width: Option<f32>, stroke: bool) {
        if !finite(&[x, y]) {
            return;
        }
        let font = self.font();
        let (width, _) = font.measure_str(text, None);
        let (_, metrics) = font.metrics();
        let dx = match self.state.align {
            Align::Start | Align::Left => 0.0,
            Align::End | Align::Right => -width,
            Align::Center => -width / 2.0,
        };
        let dy = match self.state.baseline {
            Baseline::Alphabetic => 0.0,
            Baseline::Top => -metrics.ascent,
            Baseline::Hanging => -metrics.ascent * 0.8,
            Baseline::Middle => -(metrics.ascent + metrics.descent) / 2.0,
            Baseline::Ideographic | Baseline::Bottom => -metrics.descent,
        };
        let squeeze = match max_width {
            Some(max) if max.is_finite() && max > 0.0 && width > max => max / width,
            Some(max) if !max.is_finite() || max <= 0.0 => return,
            _ => 1.0,
        };
        let mut paint = if stroke {
            let mut paint = self.paint(&self.state.stroke.clone(), false);
            paint.set_style(sk::paint::Style::Stroke);
            paint.set_stroke_width(self.state.line_width);
            paint
        } else {
            self.paint(&self.state.fill.clone(), false)
        };
        paint.set_anti_alias(true);
        let mut matrix = self.state.matrix;
        matrix.pre_translate((x, y));
        matrix.pre_scale((squeeze, 1.0), None);
        let canvas = self.surface.canvas();
        canvas.set_matrix(&matrix.into());
        canvas.draw_str(text, (dx, dy), &font, &paint);
        canvas.reset_matrix();
    }

    // Pixels.

    /// `drawImage(source, sx, sy, sw, sh, dx, dy, dw, dh)`.
    pub fn draw_image(&mut self, source: Source<'_>, from: [f32; 4], to: [f32; 4]) {
        if !finite(&from) || !finite(&to) || source.width == 0 || source.height == 0 {
            return;
        }
        let Some(image) = image_of(source) else {
            return;
        };
        let mut paint = sk::Paint::default();
        paint.set_alpha_f(self.state.alpha);
        paint.set_blend_mode(self.state.blend);
        let sampling = if self.state.smoothing {
            sk::SamplingOptions::new(sk::FilterMode::Linear, sk::MipmapMode::None)
        } else {
            sk::SamplingOptions::new(sk::FilterMode::Nearest, sk::MipmapMode::None)
        };
        let src = sk::Rect::from_xywh(from[0], from[1], from[2], from[3]);
        let dst = sk::Rect::from_xywh(to[0], to[1], to[2], to[3]);
        let matrix = self.state.matrix;
        let canvas = self.surface.canvas();
        canvas.set_matrix(&matrix.into());
        canvas.draw_image_rect_with_sampling_options(
            &image,
            Some((&src, sk::canvas::SrcRectConstraint::Fast)),
            dst,
            sampling,
            &paint,
        );
        canvas.reset_matrix();
    }

    /// `getImageData(x, y, w, h)`: straight RGBA, with what is outside the
    /// bitmap transparent black.
    pub fn get_image_data(&mut self, x: i32, y: i32, w: u32, h: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        if w == 0 || h == 0 {
            return pixels;
        }
        let info = sk::ImageInfo::new(
            (w as i32, h as i32),
            sk::ColorType::RGBA8888,
            sk::AlphaType::Unpremul,
            None,
        );
        self.surface
            .read_pixels(&info, &mut pixels, w as usize * 4, (x, y));
        pixels
    }

    /// `putImageData(data, x, y)`: straight RGBA, untouched by the transform,
    /// the clip and the alpha.
    pub fn put_image_data(&mut self, pixels: &[u8], w: u32, h: u32, x: i32, y: i32) {
        if w == 0 || h == 0 || pixels.len() < w as usize * h as usize * 4 {
            return;
        }
        let info = sk::ImageInfo::new(
            (w as i32, h as i32),
            sk::ColorType::RGBA8888,
            sk::AlphaType::Unpremul,
            None,
        );
        // Pixels that fall outside the bitmap are not written, and what does
        // fall inside is: nothing to report either way.
        let _written = self
            .surface
            .canvas()
            .write_pixels(&info, pixels, w as usize * 4, (x, y));
    }

    /// The bitmap as the page shows it: premultiplied RGBA.
    pub fn pixels(&mut self) -> Vec<u8> {
        let (w, h) = (self.width.max(1), self.height.max(1));
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        let info = sk::ImageInfo::new(
            (w as i32, h as i32),
            sk::ColorType::RGBA8888,
            sk::AlphaType::Premul,
            None,
        );
        self.surface
            .read_pixels(&info, &mut pixels, w as usize * 4, (0, 0));
        pixels
    }

    /// `toDataURL('image/png')`'s bytes.
    pub fn to_png(&mut self) -> Option<Vec<u8>> {
        let image = self.surface.image_snapshot();
        #[allow(deprecated)]
        let data = image.encode_to_data(sk::EncodedImageFormat::PNG)?;
        Some(data.as_bytes().to_vec())
    }
}

fn matrix(m: [f32; 6]) -> sk::Matrix {
    sk::Matrix::new_all(m[0], m[2], m[4], m[1], m[3], m[5], 0.0, 0.0, 1.0)
}

fn finite(values: &[f32]) -> bool {
    values.iter().all(|value| value.is_finite())
}

fn with_rule(path: &sk::Path, evenodd: bool) -> sk::Path {
    let mut path = path.clone();
    path.set_fill_type(if evenodd {
        sk::PathFillType::EvenOdd
    } else {
        sk::PathFillType::Winding
    });
    path
}

fn color4f(rgba: [f32; 4], alpha: f32) -> sk::Color4f {
    sk::Color4f::new(rgba[0], rgba[1], rgba[2], rgba[3] * alpha)
}

fn gradient_stops(stops: &[(f32, [f32; 4])], alpha: f32) -> (Vec<sk::Color4f>, Vec<f32>) {
    // A gradient with no stops paints nothing; one with a single stop is
    // that colour throughout.
    let stops: Vec<(f32, [f32; 4])> = match stops {
        [] => vec![(0.0, [0.0; 4]), (1.0, [0.0; 4])],
        [only] => vec![(0.0, only.1), (1.0, only.1)],
        many => many.to_vec(),
    };
    (
        stops
            .iter()
            .map(|(_, rgba)| color4f(*rgba, alpha))
            .collect(),
        stops
            .iter()
            .map(|(offset, _)| offset.clamp(0.0, 1.0))
            .collect(),
    )
}

fn image_of(source: Source<'_>) -> Option<sk::Image> {
    let expected = source.width as usize * source.height as usize * 4;
    if source.pixels.len() < expected {
        return None;
    }
    let info = sk::ImageInfo::new(
        (source.width as i32, source.height as i32),
        sk::ColorType::RGBA8888,
        if source.premultiplied {
            sk::AlphaType::Premul
        } else {
            sk::AlphaType::Unpremul
        },
        None,
    );
    sk::images::raster_from_data(
        &info,
        sk::Data::new_copy(&source.pixels[..expected]),
        source.width as usize * 4,
    )
}

/// How far an arc from `start` to `end` turns, in radians, positive
/// clockwise: at most a whole turn, in the direction asked (HTML §4.12.5.1.7,
/// `ellipse()`).
fn arc_sweep(start: f32, end: f32, ccw: bool) -> f32 {
    let tau = std::f32::consts::TAU;
    if !ccw && end - start >= tau {
        tau
    } else if ccw && start - end >= tau {
        -tau
    } else if ccw {
        -(start - end).rem_euclid(tau)
    } else {
        (end - start).rem_euclid(tau)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(canvas: &mut Canvas2D, x: i32, y: i32) -> [u8; 4] {
        let data = canvas.get_image_data(x, y, 1, 1);
        [data[0], data[1], data[2], data[3]]
    }

    #[test]
    fn a_filled_rectangle_is_in_the_pixels() {
        let mut canvas = Canvas2D::new(20, 20);
        canvas.set_fill(CanvasPaint::Color([1.0, 0.0, 0.0, 1.0]));
        canvas.fill_rect(5.0, 5.0, 10.0, 10.0);
        assert_eq!(pixel(&mut canvas, 10, 10), [255, 0, 0, 255]);
        assert_eq!(pixel(&mut canvas, 2, 2), [0, 0, 0, 0]);
    }

    #[test]
    fn a_path_is_where_the_transform_was_when_it_was_drawn() {
        let mut canvas = Canvas2D::new(40, 40);
        canvas.translate_for_test(20.0, 0.0);
        canvas.begin_path();
        canvas.rect(0.0, 0.0, 10.0, 10.0);
        canvas.set_transform([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        canvas.fill(false);
        assert_eq!(
            pixel(&mut canvas, 25, 5)[3],
            255,
            "moved with the transform"
        );
        assert_eq!(pixel(&mut canvas, 5, 5)[3], 0);
    }

    #[test]
    fn a_full_circle_is_drawn() {
        let mut canvas = Canvas2D::new(40, 40);
        canvas.begin_path();
        canvas.arc(20.0, 20.0, 10.0, 0.0, std::f32::consts::TAU, false);
        canvas.fill(false);
        assert_eq!(pixel(&mut canvas, 20, 20)[3], 255);
        assert_eq!(pixel(&mut canvas, 2, 2)[3], 0);
    }

    #[test]
    fn image_data_goes_in_and_comes_out_straight() {
        let mut canvas = Canvas2D::new(4, 4);
        canvas.put_image_data(&[10, 20, 30, 255], 1, 1, 1, 1);
        assert_eq!(pixel(&mut canvas, 1, 1), [10, 20, 30, 255]);
        canvas.clear_rect(0.0, 0.0, 4.0, 4.0);
        assert_eq!(pixel(&mut canvas, 1, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn save_and_restore_bring_the_state_back() {
        let mut canvas = Canvas2D::new(4, 4);
        canvas.save();
        canvas.set_line_width(5.0);
        canvas.transform([2.0, 0.0, 0.0, 2.0, 0.0, 0.0]);
        canvas.restore();
        assert_eq!(canvas.get_transform(), [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert_eq!(canvas.state.line_width, 1.0);
    }

    #[test]
    fn text_is_measured_and_drawn() {
        let mut canvas = Canvas2D::new(100, 40);
        canvas.set_font(CanvasFont {
            size: 20.0,
            ..CanvasFont::default()
        });
        let metrics = canvas.measure_text("Hello");
        assert!(metrics.width > 20.0, "{metrics:?}");
        canvas.draw_text("Hello", 5.0, 30.0, None, false);
        let drawn = canvas.get_image_data(0, 0, 100, 40);
        assert!(drawn.chunks(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn a_png_is_encoded() {
        let mut canvas = Canvas2D::new(3, 3);
        let png = canvas.to_png().expect("a png");
        assert_eq!(&png[1..4], b"PNG");
    }

    impl Canvas2D {
        fn translate_for_test(&mut self, x: f32, y: f32) {
            self.transform([1.0, 0.0, 0.0, 1.0, x, y]);
        }
    }
}
