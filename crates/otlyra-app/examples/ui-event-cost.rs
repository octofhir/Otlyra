//! Cost and frame requests of browser-chrome input with no visible mutation.
//!
//! Run with:
//!
//! ```text
//! cargo run --release -p otlyra-app --example ui-event-cost
//! ```

use std::hint::black_box;
use std::time::Instant;

use otlyra_app::browser::Browser;
use otlyra_app::fetcher::{Loaded, Loader};
use otlyra_platform::{FrameRequest, Painter, PlatformEvent};

const ITERATIONS: u64 = 1_000_000;
const SAMPLES: usize = 7;

struct NoNetwork;

impl Loader for NoNetwork {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Err(format!("the input benchmark does not fetch {url}"))
    }
}

fn main() {
    let mut browser = Browser::new(NoNetwork);

    // Leave the initial off-window pointer position. The following motion stays
    // over a blank page, where neither chrome hover nor page hover changes.
    let _ = browser.handle_event(PlatformEvent::PointerMoved { x: 320.0, y: 240.0 });

    let mut samples = Vec::with_capacity(SAMPLES);
    let mut frames = 0_u64;
    for sample in 0..=SAMPLES {
        let started = Instant::now();
        let mut sample_frames = 0_u64;
        for index in 0..ITERATIONS {
            let request = browser.handle_event(black_box(PlatformEvent::PointerMoved {
                x: 320.0 + (index & 1) as f64,
                y: 240.0,
            }));
            sample_frames += u64::from(black_box(request) != FrameRequest::None);
        }
        let ns_per_event = started.elapsed().as_secs_f64() * 1_000_000_000.0 / ITERATIONS as f64;
        if sample != 0 {
            samples.push(ns_per_event);
            frames += sample_frames;
        }
    }
    samples.sort_by(f64::total_cmp);

    println!(
        "page pointer: {SAMPLES}x{ITERATIONS} events, median {:.2} ns/event \
         (min {:.2}, max {:.2}), {frames} frame requests",
        samples[SAMPLES / 2],
        samples[0],
        samples[SAMPLES - 1],
    );
    assert_eq!(
        frames, 0,
        "motion that changes no visible state must schedule no frame"
    );
}
