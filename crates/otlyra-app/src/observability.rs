//! Tracing setup, the fixed vocabulary of span names, and the journal.
//!
//! `log` cannot answer "which frame?" or "which navigation?"; spans can.
//!
//! # The journal
//!
//! The browser already says a great deal about itself, and until now all of it
//! went to a terminal nobody has open. The journal is the same stream kept where
//! the browser can read it: a bounded ring of what was said and how long each
//! stage took. It is written by a tracing layer and read by the inspector, so
//! there is one account of what happened rather than a second one built for the
//! panel — a panel with its own instrumentation would be a second set of numbers
//! to keep agreeing with the first.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

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

/// How many lines the journal keeps before the oldest goes.
///
/// Bounded because a browser left open overnight says a great deal, and an
/// unbounded log is a memory leak with a good excuse.
const RECORD_LIMIT: usize = 500;
/// How many finished stages it keeps, which is a few frames' worth.
const TIMING_LIMIT: usize = 200;

/// One thing the browser said about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// How loud it was.
    pub level: tracing::Level,
    /// Which module said it.
    pub target: String,
    /// What it said.
    pub message: String,
}

/// How long one stage of the pipeline took.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Which stage, from [`spans`].
    pub span: &'static str,
    /// How long it was open.
    pub took: Duration,
}

/// What the browser has said and how long its stages took.
///
/// Shared and bounded. Cloning one is cloning a handle: there is one journal per
/// process, because there is one browser saying things.
#[derive(Clone, Default)]
pub struct Journal {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    records: VecDeque<Record>,
    timings: VecDeque<Timing>,
}

impl Journal {
    /// Everything said, oldest first.
    pub fn records(&self) -> Vec<Record> {
        self.inner
            .lock()
            .map(|inner| inner.records.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Every stage that finished, oldest first.
    pub fn timings(&self) -> Vec<Timing> {
        self.inner
            .lock()
            .map(|inner| inner.timings.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The most recent time each stage took.
    ///
    /// What a frame line is made of: the last measurement of each stage rather
    /// than every measurement of it, because the question is *what is slow now*.
    pub fn latest(&self) -> Vec<Timing> {
        let timings = self.timings();
        spans::ALL
            .iter()
            .filter_map(|name| {
                timings
                    .iter()
                    .rev()
                    .find(|timing| timing.span == *name)
                    .copied()
            })
            .collect()
    }

    /// Forget everything, which is what a person means by clearing a console.
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.records.clear();
            inner.timings.clear();
        }
    }

    fn push_record(&self, record: Record) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.records.len() >= RECORD_LIMIT {
                inner.records.pop_front();
            }
            inner.records.push_back(record);
        }
    }

    fn push_timing(&self, timing: Timing) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.timings.len() >= TIMING_LIMIT {
                inner.timings.pop_front();
            }
            inner.timings.push_back(timing);
        }
    }
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Journal")
            .field("records", &self.records().len())
            .field("timings", &self.timings().len())
            .finish()
    }
}

/// The process's journal.
///
/// A global because tracing's subscriber is one, and a journal the browser had
/// to be handed would have to reach every place that says anything — which is
/// every crate, including the ones that have never heard of a browser.
pub fn journal() -> &'static Journal {
    static JOURNAL: LazyLock<Journal> = LazyLock::new(Journal::default);
    &JOURNAL
}

/// The layer that writes what is said into the journal.
struct Recorder;

/// When a span was opened, kept on the span itself.
struct OpenedAt(Instant);

impl<S> Layer<S> for Recorder
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        _attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        context: Context<'_, S>,
    ) {
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(OpenedAt(Instant::now()));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        journal().push_record(Record {
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            message: message.0,
        });
    }

    fn on_close(&self, id: tracing::Id, context: Context<'_, S>) {
        let Some(span) = context.span(&id) else {
            return;
        };
        let Some(opened) = span.extensions().get::<OpenedAt>().map(|at| at.0) else {
            return;
        };
        // Only the named stages. A span from a dependency is a span nothing in
        // this browser has a name for, and a frame line built from those would
        // be a list of numbers about somebody else's work.
        let Some(name) = spans::ALL.iter().find(|known| **known == span.name()) else {
            return;
        };
        journal().push_timing(Timing {
            span: name,
            took: opened.elapsed(),
        });
    }
}

/// Collects a tracing event's `message` field, and anything else it carries.
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        // The message field is the sentence; everything else is `name=value`,
        // which is how it reads in a terminal and how a person expects to search
        // it.
        if field.name() == "message" {
            self.0.push_str(format!("{value:?}").trim_matches('"'));
        } else {
            self.0.push_str(&format!("{}={value:?}", field.name()));
        }
    }
}

/// Install the global tracing subscriber.
///
/// Returns `false` if a subscriber was already installed, which happens in tests
/// and is not an error worth failing a browser launch over.
pub fn init() -> bool {
    let filter = EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new("warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                // Every span in `spans` is a stage of the pipeline, and the
                // question asked of them is always "how long did it take" — so
                // closing a span reports it. Without this the span names are
                // labels on nothing.
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
                .with_target(true)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .with_writer(std::io::stderr),
        )
        // Beneath the filter, so what the terminal shows and what the panel
        // shows are the same stream and `OTLYRA_LOG` governs both.
        .with(Recorder)
        .try_init()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::{Journal, Record, Timing, spans};
    use std::time::Duration;

    fn said(journal: &Journal, message: &str) {
        journal.push_record(Record {
            level: tracing::Level::INFO,
            target: "otlyra_app::test".to_owned(),
            message: message.to_owned(),
        });
    }

    #[test]
    fn the_journal_keeps_what_was_said_in_the_order_it_was_said() {
        let journal = Journal::default();
        said(&journal, "first");
        said(&journal, "second");

        let messages: Vec<String> = journal
            .records()
            .into_iter()
            .map(|record| record.message)
            .collect();
        assert_eq!(messages, ["first", "second"]);
    }

    #[test]
    fn the_journal_forgets_the_oldest_rather_than_growing_forever() {
        let journal = Journal::default();
        for index in 0..super::RECORD_LIMIT + 10 {
            said(&journal, &index.to_string());
        }
        let records = journal.records();
        assert_eq!(records.len(), super::RECORD_LIMIT);
        // A browser left open overnight says a great deal, and what it said
        // first is what nobody is looking for.
        assert_eq!(records[0].message, "10");
    }

    #[test]
    fn the_frame_line_is_the_last_of_each_stage_and_not_every_one() {
        let journal = Journal::default();
        for micros in [900, 1200, 300] {
            journal.push_timing(Timing {
                span: spans::LAYOUT,
                took: Duration::from_micros(micros),
            });
        }
        journal.push_timing(Timing {
            span: spans::PAINT,
            took: Duration::from_micros(70),
        });

        let latest = journal.latest();
        assert_eq!(latest.len(), 2, "one per stage that ran: {latest:?}");
        let layout = latest
            .iter()
            .find(|timing| timing.span == spans::LAYOUT)
            .expect("layout ran");
        assert_eq!(
            layout.took,
            Duration::from_micros(300),
            "the question is what is slow now, not what was slow once"
        );

        // In pipeline order, so the stage that is out of proportion is found by
        // reading along it.
        assert_eq!(latest[0].span, spans::LAYOUT);
        assert_eq!(latest[1].span, spans::PAINT);
    }

    /// The layer, driven by a subscriber of its own so the global one is left
    /// alone — every test in this binary shares that.
    #[test]
    fn the_layer_writes_what_was_said_and_how_long_a_stage_took() {
        use tracing_subscriber::layer::SubscriberExt;

        super::journal().clear();
        let subscriber = tracing_subscriber::registry().with(super::Recorder);
        tracing::subscriber::with_default(subscriber, || {
            let error = "no such file";
            tracing::warn!(%error, "navigation failed");
            let span = tracing::info_span!(spans::LAYOUT);
            span.in_scope(|| {});
        });

        let records = super::journal().records();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].level, tracing::Level::WARN);
        // Both halves of what was said: the sentence, and the fields that make
        // it worth reading. A line that dropped its fields would be a line
        // saying something failed without saying what.
        assert!(
            records[0].message.contains("navigation failed"),
            "{:?}",
            records[0].message
        );
        assert!(
            records[0].message.contains("no such file"),
            "the fields came with it: {:?}",
            records[0].message
        );

        let timings = super::journal().timings();
        assert_eq!(timings.len(), 1, "one named stage opened and closed");
        assert_eq!(timings[0].span, spans::LAYOUT);
    }

    #[test]
    fn clearing_the_journal_clears_both_halves_of_it() {
        let journal = Journal::default();
        said(&journal, "something");
        journal.push_timing(Timing {
            span: spans::PAINT,
            took: Duration::from_micros(1),
        });

        journal.clear();
        assert!(journal.records().is_empty());
        assert!(journal.timings().is_empty());
    }

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
