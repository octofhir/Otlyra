//! `console` from a page, arriving where the rest of our logging arrives.
//!
//! The engine's default sink prints to the process's stdout and stderr, which
//! is right for a command-line runtime and wrong for a browser: a page would be
//! writing on the terminal the browser was started from, with nothing to say
//! which page it was, and nothing a devtools console could later read. Here it
//! becomes a `tracing` event under one target, so it can be filtered on,
//! captured in a test, and — when there is a devtools console — subscribed to
//! by a second subscriber without this code changing.

use std::sync::Arc;

use otter_runtime::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle};

/// The `tracing` target every console event from page script carries.
///
/// Named rather than derived from the module path so a filter written against
/// it (`RUST_LOG=page.console=debug`) keeps working when this file moves.
pub const CONSOLE_TARGET: &str = "page.console";

/// A console sink that writes page output into `tracing`.
#[derive(Debug, Default)]
pub struct TracingConsole;

impl ConsoleSink for TracingConsole {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        // The engine has already rendered every argument in JavaScript
        // argument order, so the join is the whole of the formatting: a
        // `console.log("a", 1)` is one line, the way it is in every browser.
        let line = fields.join(" ");
        match level {
            ConsoleLevel::Log | ConsoleLevel::Info => {
                tracing::info!(target: CONSOLE_TARGET, "{line}");
            }
            ConsoleLevel::Debug => tracing::debug!(target: CONSOLE_TARGET, "{line}"),
            // `console.trace` is a stack dump in a browser and there is nothing
            // yet that could produce one here, so it is the lowest level we
            // have rather than a warning about nothing.
            ConsoleLevel::Trace => tracing::trace!(target: CONSOLE_TARGET, "{line}"),
            ConsoleLevel::Warn => tracing::warn!(target: CONSOLE_TARGET, "{line}"),
            // A failed assert is the page saying something it believed is not
            // true. That is an error in the page, and reads as one.
            ConsoleLevel::Error | ConsoleLevel::Assert => {
                tracing::error!(target: CONSOLE_TARGET, "{line}");
            }
            // The engine's level set is `non_exhaustive` and it is the engine's
            // to grow. A level we have not heard of is still page output and
            // still belongs in the log, at the level nothing is lost from.
            _ => tracing::info!(target: CONSOLE_TARGET, "{line}"),
        }
    }
}

/// The console handle a page isolate is built with.
#[must_use]
pub fn tracing_console() -> ConsoleSinkHandle {
    Arc::new(TracingConsole)
}
