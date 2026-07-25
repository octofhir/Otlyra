//! Comparing a rendered PNG against a committed golden.
//!
//! Shared by every golden test, so they cannot disagree about what "matches"
//! means. Rasterization is deterministic for identical input on identical Skia,
//! so the tolerance is tight on purpose: it absorbs nothing but a last-bit
//! rounding difference.
//!
//! Regenerate goldens deliberately:
//!
//! ```sh
//! OTLYRA_UPDATE_GOLDEN=1 cargo test -p otlyra-app --test golden_image --test interface_golden
//! ```

use std::path::{Path, PathBuf};

/// Per-channel tolerance.
const TOLERANCE: u8 = 1;

/// Where the goldens live.
pub fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("goldens")
}

/// Decode a PNG into its dimensions and RGBA bytes.
pub fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().expect("valid PNG header");
    let mut buffer = vec![0; reader.output_buffer_size().expect("known buffer size")];
    let info = reader.next_frame(&mut buffer).expect("valid PNG body");
    buffer.truncate(info.buffer_size());
    (info.width, info.height, buffer)
}

/// Compare `rendered` to the golden at `path`, or rewrite the golden when
/// `OTLYRA_UPDATE_GOLDEN` is set.
pub fn assert_matches_golden(rendered: &[u8], path: &Path) {
    if std::env::var_os("OTLYRA_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("goldens directory")).expect("create dir");
        std::fs::write(path, rendered).expect("write golden");
        eprintln!("updated golden: {}", path.display());
        return;
    }

    let expected = std::fs::read(path).unwrap_or_else(|error| {
        panic!(
            "missing golden {}: {error}\nregenerate with OTLYRA_UPDATE_GOLDEN=1",
            path.display()
        )
    });

    let (golden_width, golden_height, golden_pixels) = decode(&expected);
    let (width, height, pixels) = decode(rendered);

    assert_eq!(
        (width, height),
        (golden_width, golden_height),
        "rendered frame is {width}x{height}, golden {} is {golden_width}x{golden_height}",
        path.display()
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
        differing,
        0,
        "{differing} channel samples of {} differ by more than {TOLERANCE}; \
         worst delta {worst} at ({}, {})\nregenerate with OTLYRA_UPDATE_GOLDEN=1 if intended",
        path.display(),
        worst_at.0,
        worst_at.1
    );
}
