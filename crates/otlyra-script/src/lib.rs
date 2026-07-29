//! # otlyra-script — the page's own code, and the leash it runs on
//!
//! ## Purpose
//!
//! A page brings code with it, and that code is the one thing on a page that is
//! written by somebody who is not us and does not wish us well. This crate is
//! where it runs: one [Otter](https://github.com/octofhir/otter) isolate per
//! page, built with a deny-everything capability set, a console that goes to
//! our own log rather than the process's stdout, and a watchdog that can stop a
//! script that will not stop itself.
//!
//! ## Contents
//!
//! - [`capabilities`] — what page script is allowed to ask the host for, which
//!   is nothing.
//! - [`console`] — `console.log` from a page, arriving in `tracing`.
//! - [`host`] — the isolate itself, its lifetime, and running one script in it.
//!
//! ## Invariants
//!
//! 1. **A page script has no capabilities.** Not filesystem, not network, not
//!    environment, not subprocesses, not FFI. Everything a page is *supposed*
//!    to reach — a `fetch`, a stylesheet, storage — will arrive later as a host
//!    object we wrote and gated ourselves, never as an engine capability.
//! 2. **A script that does not return is stopped.** The engine's interrupt flag
//!    is tripped from another thread after a fixed budget, so a `while (true)`
//!    costs the page its script and not the browser.
//! 3. **A failing script is reported, not swallowed, and never aborts the
//!    parse.** A syntax error in one `<script>` is a diagnostic; the document
//!    around it still becomes a page.
//! 4. **Nothing in this crate knows what a DOM is yet.** The document, its
//!    nodes and their bindings arrive at M13; what is here is the seam and the
//!    leash, which have to be right before there is anything worth reaching.

// Mandatory linking convention: every path `otter-macros` generates is
// `::otter_vm::…`, and `otter-runtime` re-exports everything those paths name.
extern crate otter_runtime as otter_vm;

pub mod capabilities;
pub mod console;
pub mod dom;
pub mod timers;
pub mod host;
pub mod page;

pub use host::{ScriptError, ScriptHost, ScriptOutcome};
pub use page::PageScripts;
