//! Count what differs between two pictures.
//!
//! A comparison a person makes by looking is a comparison nobody runs twice. This
//! turns two PNGs into a number — how many pixels differ, and by how much — and
//! writes a third picture with the differences marked, so that when the number is
//! not zero there is somewhere to look.
//!
//! ```text
//! cargo run -p otlyra-gfx --example compare -- ours.png theirs.png difference.png
//! ```
//!
//! Exit code is 0 when the two agree within the tolerance and 1 when they do not,
//! so this can be a gate rather than a report.

use std::process::ExitCode;

/// How far apart one channel may be before a pixel counts as different.
///
/// Not zero: two rasterizers that agree about a shape still disagree about the
/// last bit of an antialiased edge, and a comparison that calls that a difference
/// reports every picture as broken.
const TOLERANCE: u8 = 8;

/// The share of pixels that may differ before the two are called different.
const ALLOWED: f64 = 0.002;

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let (Some(ours), Some(theirs)) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: compare <ours.png> <theirs.png> [difference.png]");
        return ExitCode::FAILURE;
    };
    let difference_path = arguments.next();

    let (ours, theirs) = match (read(&ours), read(&theirs)) {
        (Ok(ours), Ok(theirs)) => (ours, theirs),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("compare: {error}");
            return ExitCode::FAILURE;
        }
    };

    if ours.width != theirs.width || ours.height != theirs.height {
        println!(
            "different sizes: {}×{} against {}×{}",
            ours.width, ours.height, theirs.width, theirs.height
        );
        return ExitCode::FAILURE;
    }

    let mut differing = 0u64;
    let mut worst = 0u8;
    let mut marked = vec![0u8; ours.pixels.len()];

    for (index, (ours, theirs)) in ours
        .pixels
        .chunks_exact(4)
        .zip(theirs.pixels.chunks_exact(4))
        .enumerate()
    {
        let apart = (0..3)
            .map(|channel| ours[channel].abs_diff(theirs[channel]))
            .max()
            .unwrap_or(0);
        worst = worst.max(apart);

        let at = index * 4;
        if apart > TOLERANCE {
            differing += 1;
            // Where they differ, in red; where they agree, the picture faintly, so
            // the differences have something to sit on.
            marked[at..at + 4].copy_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
        } else {
            let faint = |value: u8| (u16::from(value) / 3 + 170) as u8;
            marked[at] = faint(ours[0]);
            marked[at + 1] = faint(ours[1]);
            marked[at + 2] = faint(ours[2]);
            marked[at + 3] = 0xFF;
        }
    }

    let total = u64::from(ours.width) * u64::from(ours.height);
    let share = differing as f64 / total.max(1) as f64;
    println!(
        "{differing} of {total} pixels differ ({:.3}%), worst channel {worst}",
        share * 100.0
    );

    if let Some(path) = difference_path
        && let Err(error) = write(&path, ours.width, ours.height, &marked)
    {
        eprintln!("compare: {error}");
        return ExitCode::FAILURE;
    }

    if share > ALLOWED {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// One decoded picture.
struct Picture {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

fn read(path: &str) -> Result<Picture, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    let image = otlyra_gfx::decode_image(&bytes).map_err(|error| format!("{path}: {error}"))?;
    Ok(Picture {
        width: image.width,
        height: image.height,
        pixels: image.data.as_ref().to_vec(),
    })
}

fn write(path: &str, width: u32, height: u32, pixels: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|error| format!("{path}: {error}"))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(pixels))
        .map_err(|error| format!("{path}: {error}"))
}
