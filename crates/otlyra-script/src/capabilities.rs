//! What a page's script may ask the host for, which is nothing.
//!
//! Otter's capability set is an embedder's answer to "may this code read a
//! file, open a socket, read the environment, start a process, load a native
//! library". For a page, every one of those answers is no, and the set alone
//! would say so. The hook beside it exists because a set is data and a browser
//! wants an event: a page that reaches for one of these is either broken or
//! hostile, and either way it is worth a line in the log rather than a silent
//! `false`.
//!
//! Everything a page is legitimately supposed to reach — a `fetch`, a
//! stylesheet, storage, a cookie — is *not* an engine capability and never
//! becomes one. It arrives as a host object we wrote, gated by our own origin
//! policy, over resources our own network layer already fetched. The engine's
//! own idea of "net" stays denied for the life of the browser.

use otter_runtime::{CapabilityRequest, CapabilitySet, RuntimeCapability, RuntimeCapabilityHook};

/// The capability set every page isolate is built with: deny-all.
#[must_use]
pub fn page_capabilities() -> CapabilitySet {
    CapabilitySet::sandbox()
}

/// A capability hook that refuses everything and says so.
///
/// Installed beside [`page_capabilities`] rather than instead of it: the set is
/// the decision, this is the record of the attempt.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyPageCapabilities;

impl RuntimeCapabilityHook for DenyPageCapabilities {
    fn check_capability(
        &self,
        _capabilities: &CapabilitySet,
        capability: RuntimeCapability,
        request: &CapabilityRequest<'_>,
    ) -> bool {
        // Debug rather than a warning, because most of these are not the page.
        // The engine's own bootstrap asks for every environment variable it can
        // name while the isolate is being built, and a browser that logged a
        // warning per variable would print a screenful before the first byte of
        // the document was parsed. What matters is that the answer is no.
        tracing::debug!(
            target: "page.capability",
            ?capability,
            ?request,
            "denied a host capability to page script",
        );
        false
    }
}
