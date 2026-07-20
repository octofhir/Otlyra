//! Tracing setup, and the fixed vocabulary of span names.
//!
//! `log` cannot answer "which frame?" or "which navigation?"; spans can.

use tracing_subscriber::EnvFilter;

/// Filtering, in `RUST_LOG` syntax. Project-specific rather than `RUST_LOG` so that
/// turning on our tracing does not turn on every dependency's.
pub const LOG_ENV: &str = "OTLYRA_LOG";

/// Span names, fixed once and never renamed.
///
/// Every performance target, profiling script and dashboard is stated in terms of
/// these. Renaming one silently invalidates historical measurements. New spans get
/// added to this list; existing ones do not change.
pub mod spans {
    /// A navigation, from the request for a URL to first present.
    pub const NAVIGATION: &str = "navigation";
    /// Fetching one resource: document, stylesheet, script or image.
    pub const RESOURCE_LOAD: &str = "resource_load";
    /// HTML tokenization and tree construction.
    pub const PARSE_HTML: &str = "parse_html";
    /// Selector matching and the cascade.
    pub const RECALC_STYLE: &str = "recalc_style";
    /// Box-tree construction through to the fragment tree.
    pub const LAYOUT: &str = "layout";
    /// Lowering the fragment tree into a display list.
    pub const BUILD_DISPLAY_LIST: &str = "build_display_list";
    /// Replaying a display list into a `PaintTarget`.
    pub const PAINT: &str = "paint";
    /// Getting rasterized pixels onto the screen.
    pub const PRESENT: &str = "present";
    /// Evaluating script.
    pub const SCRIPT_EVAL: &str = "script_eval";
    /// Draining the microtask queue after a task.
    pub const MICROTASK_CHECKPOINT: &str = "microtask_checkpoint";

    /// Every span name, in pipeline order.
    pub const ALL: &[&str] = &[
        NAVIGATION,
        RESOURCE_LOAD,
        PARSE_HTML,
        RECALC_STYLE,
        LAYOUT,
        BUILD_DISPLAY_LIST,
        PAINT,
        PRESENT,
        SCRIPT_EVAL,
        MICROTASK_CHECKPOINT,
    ];
}

/// Install the global tracing subscriber.
///
/// Returns `false` if a subscriber was already installed, which happens in tests
/// and is not an error worth failing a browser launch over.
pub fn init() -> bool {
    let filter = EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new("warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Every span in `spans` is a stage of the pipeline, and the question asked
        // of them is always "how long did it take" — so closing a span reports it.
        // Without this the span names are labels on nothing.
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_target(true)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .try_init()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::spans;

    #[test]
    fn span_names_are_unique_and_lowercase_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for name in spans::ALL {
            assert!(seen.insert(*name), "duplicate span name: {name}");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "span names are lower_snake_case: {name}"
            );
        }
        assert_eq!(seen.len(), spans::ALL.len());
    }

    /// The core pipeline names. Editing this test is editing a public contract.
    #[test]
    fn the_core_pipeline_spans_are_all_present() {
        for expected in [
            "navigation",
            "parse_html",
            "recalc_style",
            "layout",
            "build_display_list",
            "paint",
            "present",
        ] {
            assert!(
                spans::ALL.contains(&expected),
                "span {expected} went missing"
            );
        }
    }
}
