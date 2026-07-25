//! The acceptance corpus for a new `PaintTarget` backend.
//!
//! A shapes-only renderer proves almost nothing about a browser. This list
//! deliberately crosses every operation the live display list can ask for:
//! nested clip/layer state, opacity and blending, solid and gradient fills,
//! arbitrary path stroke, blur, glyphs (sharp and blurred), image sampling, and
//! a hit-test item that must paint nothing.
//!
//! The current Skia backend writes the reference images and reports cold and
//! steady replay cost. A candidate direct-GPU backend must replay this exact
//! list at both scales, compare through the `compare` example, and improve the
//! measured browser scenarios before it can replace the live default.
//!
//! ```text
//! cargo run --release -p otlyra-gfx --example backend-corpus -- /tmp/backend-corpus
//! cargo run -p otlyra-gfx --example compare -- \
//!   /tmp/backend-corpus/skia-1x.png /tmp/backend-corpus/metal-1x.png difference-1x.png
//! cargo run -p otlyra-gfx --example compare -- \
//!   /tmp/backend-corpus/skia-2x.png /tmp/backend-corpus/metal-2x.png difference-2x.png
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use otlyra_gfx::kurbo::{Affine, BezPath, Point, Rect, RoundedRect, Shape, Stroke};
use otlyra_gfx::peniko::{
    Blob, Brush, Color, ColorStop, Extend, Fill, FontData, Gradient, ImageAlphaType, ImageData,
    ImageFormat, ImageSampler, Mix,
};
use otlyra_gfx::{
    DisplayItem, DisplayList, Glyph, HitTestId, ImageResource, PaintOp, PaintTarget as _,
    RecordingPainter, SkiaPainter, render,
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 400;
const STEADY_FRAMES: u32 = 100;

fn main() {
    let directory = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/otlyra-backend-corpus".to_owned());
    std::fs::create_dir_all(&directory).expect("create output directory");

    let logical = corpus();
    prove_contract(&logical);
    println!("{} logical display items", logical.len());

    for scale in [1_u32, 2] {
        let mut list = logical.clone();
        list.transform(Affine::scale(f64::from(scale)));
        let width = WIDTH * scale;
        let height = HEIGHT * scale;
        let (png, cold, steady_p50, steady_p95) = replay(&list, width, height);
        let path = format!("{directory}/skia-{scale}x.png");
        std::fs::write(&path, png).expect("write reference PNG");
        println!(
            "CPU {scale}x: cold {cold:?}, steady p50 {steady_p50:?}, \
             p95 {steady_p95:?}, {path}"
        );
    }

    #[cfg(target_os = "macos")]
    replay_metal(&logical, &directory);
}

/// Rasterize once cold and then repeatedly into the same backend resources.
fn replay(list: &DisplayList, width: u32, height: u32) -> (Vec<u8>, Duration, Duration, Duration) {
    let mut target = SkiaPainter::new_raster(width, height).expect("a raster surface");

    let started = Instant::now();
    target.reset();
    render(list, &mut target);
    let cold = started.elapsed();

    let mut steady = Vec::with_capacity(STEADY_FRAMES as usize);
    for _ in 0..STEADY_FRAMES {
        let started = Instant::now();
        target.reset();
        render(list, &mut target);
        steady.push(started.elapsed());
    }
    steady.sort_unstable();
    let png = target.encode_png().expect("encode reference PNG");
    (
        png,
        cold,
        percentile(&steady, 0.50),
        percentile(&steady, 0.95),
    )
}

fn percentile(samples: &[Duration], quantile: f64) -> Duration {
    let index = ((samples.len() - 1) as f64 * quantile).round() as usize;
    samples[index]
}

/// Replay the exact same corpus directly into wgpu-owned Metal textures.
///
/// Skia and wgpu use the same `MTLDevice` and `MTLCommandQueue`. The PNG encode
/// reads pixels only to make parity inspectable; the render itself has no CPU
/// raster surface, readback, or texture upload.
#[cfg(target_os = "macos")]
fn replay_metal(logical: &DisplayList, directory: &str) {
    let setup_started = Instant::now();
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::METAL;
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }))
    .expect("a Metal adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("otlyra-backend-corpus"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    }))
    .expect("a Metal device");
    println!("Metal adapter/device: {:?}", setup_started.elapsed());

    for scale in [1_u32, 2] {
        let mut list = logical.clone();
        list.transform(Affine::scale(f64::from(scale)));
        let width = WIDTH * scale;
        let height = HEIGHT * scale;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("otlyra-backend-corpus"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let wrap_started = Instant::now();
        // SAFETY: the guards name objects from the same wgpu Metal device. The
        // texture and wgpu device/queue below outlive `target`, and no wgpu work
        // touches the texture while Skia records into it.
        let mut target = unsafe {
            let hal_device = device
                .as_hal::<wgpu::hal::api::Metal>()
                .expect("wgpu selected Metal");
            let hal_queue = queue
                .as_hal::<wgpu::hal::api::Metal>()
                .expect("wgpu selected Metal");
            let hal_texture = texture
                .as_hal::<wgpu::hal::api::Metal>()
                .expect("wgpu selected Metal");
            SkiaPainter::wrap_metal_texture(
                width,
                height,
                &**hal_device.raw_device() as *const _ as *mut std::ffi::c_void,
                hal_queue.as_raw() as *const _ as *mut std::ffi::c_void,
                hal_texture.raw_handle() as *const _ as *mut std::ffi::c_void,
            )
            .expect("wrap the wgpu Metal texture")
        };
        let wrap = wrap_started.elapsed();

        let started = Instant::now();
        target.reset();
        render(&list, &mut target);
        target.sync_gpu();
        let cold = started.elapsed();

        let mut steady = Vec::with_capacity(STEADY_FRAMES as usize);
        for _ in 0..STEADY_FRAMES {
            let started = Instant::now();
            target.reset();
            render(&list, &mut target);
            target.sync_gpu();
            steady.push(started.elapsed());
        }
        steady.sort_unstable();

        let path = format!("{directory}/metal-{scale}x.png");
        std::fs::write(
            &path,
            target.encode_png().expect("encode Metal reference PNG"),
        )
        .expect("write Metal reference PNG");
        println!(
            "Metal {scale}x: wrap {wrap:?}, cold {cold:?}, steady p50 {:?}, \
             p95 {:?}, {path}",
            percentile(&steady, 0.50),
            percentile(&steady, 0.95),
        );
    }
}

/// Assert that the corpus reaches every operation through the backend seam.
fn prove_contract(list: &DisplayList) {
    let mut target = RecordingPainter::new();
    render(list, &mut target);
    let ops = target.ops();

    assert!(ops.iter().any(|op| matches!(op, PaintOp::PushLayer { .. })));
    assert!(ops.iter().any(|op| matches!(op, PaintOp::PopLayer)));
    assert!(ops.iter().any(|op| matches!(op, PaintOp::Fill { .. })));
    assert!(ops.iter().any(|op| matches!(op, PaintOp::Stroke { .. })));
    assert!(
        ops.iter()
            .any(|op| matches!(op, PaintOp::FillBlurred { .. }))
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
    );
    assert!(ops.iter().any(|op| matches!(op, PaintOp::DrawImage { .. })));
}

/// One compact scene that is difficult for a fake browser backend to pass.
fn corpus() -> DisplayList {
    let mut list = DisplayList::new();
    fill(
        &mut list,
        Rect::new(0.0, 0.0, f64::from(WIDTH), f64::from(HEIGHT)),
        Brush::Solid(Color::from_rgb8(0xf4, 0xf5, 0xf8)),
    );

    let card = RoundedRect::new(28.0, 26.0, 612.0, 374.0, 18.0).to_path(0.1);
    list.push(DisplayItem::Blurred {
        transform: Affine::IDENTITY,
        brush: Brush::Solid(Color::from_rgba8(0x20, 0x28, 0x38, 0x55)),
        blur: 18.0,
        shape: card.clone(),
    });
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(Color::WHITE),
        brush_transform: None,
        shape: card,
    });

    let clip = RoundedRect::new(48.0, 48.0, 592.0, 180.0, 14.0).to_path(0.1);
    list.push(DisplayItem::PushLayer {
        blend: Mix::Multiply.into(),
        alpha: 0.82,
        transform: Affine::IDENTITY,
        clip,
    });
    let mut gradient = Gradient::new_linear(Point::new(48.0, 48.0), Point::new(592.0, 180.0));
    gradient.extend = Extend::Pad;
    gradient.stops.push(ColorStop {
        offset: 0.0,
        color: Color::from_rgb8(0x42, 0x7a, 0xe8).into(),
    });
    gradient.stops.push(ColorStop {
        offset: 0.48,
        color: Color::from_rgb8(0x8b, 0x5c, 0xd6).into(),
    });
    gradient.stops.push(ColorStop {
        offset: 1.0,
        color: Color::from_rgb8(0xeb, 0x6f, 0x92).into(),
    });
    fill(
        &mut list,
        Rect::new(36.0, 36.0, 606.0, 194.0),
        Brush::Gradient(gradient),
    );
    list.push(DisplayItem::PopLayer);

    let mut curve = BezPath::new();
    curve.move_to((70.0, 250.0));
    curve.curve_to((150.0, 178.0), (238.0, 328.0), (318.0, 246.0));
    curve.curve_to((390.0, 174.0), (492.0, 326.0), (566.0, 238.0));
    list.push(DisplayItem::Stroke {
        style: Stroke::new(6.0),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(Color::from_rgb8(0x24, 0x2a, 0x38)),
        brush_transform: None,
        shape: curve,
    });

    let image = checkerboard();
    list.push(DisplayItem::Image {
        image: ImageResource::from(image.clone()),
        sampler: ImageSampler::default(),
        transform: Affine::translate((82.0, 278.0)) * Affine::scale(40.0),
        clip_rect: Some(Rect::new(0.0, 0.0, 2.0, 2.0)),
    });
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Image(otlyra_gfx::peniko::ImageBrush::new(image).with_extend(Extend::Repeat)),
        brush_transform: Some(Affine::scale(8.0)),
        shape: RoundedRect::new(188.0, 286.0, 300.0, 350.0, 9.0).to_path(0.1),
    });

    let font = FontData::new(
        Blob::new(Arc::new(
            include_bytes!("../../otlyra-text/fonts/Roboto-Regular.ttf").to_vec(),
        )),
        0,
    );
    // Roboto's glyph ids for "Otlyra". Keeping ids here is intentional: shaping
    // belongs above `otlyra-gfx`; the backend contract starts with shaped glyphs.
    let glyphs = [
        (52_u32, 0.0),
        (89, 31.0),
        (81, 47.0),
        (94, 58.0),
        (87, 80.0),
        (70, 96.0),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (id, x))| Glyph {
        id,
        x,
        y: 0.0,
        text_offset: index as u32,
    })
    .collect::<Vec<_>>();
    list.push_glyph_run(
        &font,
        42.0,
        Vec::new(),
        Brush::Solid(Color::from_rgba8(0x24, 0x2a, 0x38, 0x66)),
        Affine::translate((342.0, 334.0)),
        false,
        6.0,
        glyphs.clone(),
    );
    list.push_glyphs(
        &font,
        42.0,
        Vec::new(),
        Brush::Solid(Color::from_rgb8(0x24, 0x2a, 0x38)),
        Affine::translate((336.0, 328.0)),
        true,
        glyphs,
    );

    list.push(DisplayItem::HitTest {
        rect: Rect::new(48.0, 48.0, 592.0, 180.0),
        transform: Affine::IDENTITY,
        id: HitTestId(1),
    });
    list
}

fn fill(list: &mut DisplayList, rect: Rect, brush: Brush) {
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush,
        brush_transform: None,
        shape: rect.to_path(0.1),
    });
}

fn checkerboard() -> ImageData {
    ImageData {
        data: Blob::new(Arc::new(vec![
            0xff, 0xb4, 0x3c, 0xff, 0x23, 0x2a, 0x38, 0xff, 0x23, 0x2a, 0x38, 0xff, 0xff, 0xb4,
            0x3c, 0xff,
        ])),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::AlphaPremultiplied,
        width: 2,
        height: 2,
    }
}
