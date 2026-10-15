//! One shared monotonic timebase, with a navigation origin for each document.
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Timebase {
    instant: Instant,
    unix_ticks: i128,
}

fn timebase() -> &'static Timebase {
    static BASE: OnceLock<Timebase> = OnceLock::new();
    BASE.get_or_init(|| {
        let instant = Instant::now();
        let unix_ticks = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => (duration.as_nanos() / 100_000) as i128,
            Err(error) => -(error.duration().as_nanos().div_ceil(100_000) as i128),
        };
        Timebase {
            instant,
            unix_ticks,
        }
    })
}

// A shared 100-microsecond grid; origins can precede first clock construction
// when a document spent time fetching before its script environment existed.
fn ticks(at: Instant) -> i128 {
    let base = timebase().instant;
    if at >= base {
        (at.duration_since(base).as_nanos() / 100_000) as i128
    } else {
        -(base.duration_since(at).as_nanos().div_ceil(100_000) as i128)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PageClock {
    origin: i128,
}

impl PageClock {
    pub(crate) fn new() -> Self {
        Self::at(Instant::now())
    }
    pub(crate) fn at(at: Instant) -> Self {
        Self { origin: ticks(at) }
    }
    pub(crate) fn relative(self, at: Instant) -> f64 {
        (ticks(at) - self.origin).max(0) as f64 / 10.0
    }
    pub(crate) fn now(self) -> f64 {
        self.relative(Instant::now())
    }
    pub(crate) fn time_origin(self) -> f64 {
        (timebase().unix_ticks + self.origin) as f64 / 10.0
    }
}
