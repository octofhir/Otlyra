//! # otlyra-net — bytes off the network
//!
//! ## Purpose
//!
//! Turn a URL into bytes, safely. This crate knows about HTTP, TLS, redirects,
//! resource limits and character encodings. It knows nothing about documents:
//! there is no DOM, style, layout, paint or script type anywhere in its build
//! graph, and there never will be.
//!
//! ## Contents
//!
//! - [`limits`] — the caps every response is measured against.
//! - [`url`] — turning what the user typed into a URL we are willing to fetch.
//! - [`loader`] — the shared client and the fetch itself.
//!
//! ## Invariants
//!
//! 1. **Everything crossing this crate's boundary is owned and `Send`.** No
//!    borrowed slice, no `Arc` into our own state, no handle. That is what lets
//!    the loader move to another thread — and later another process — without
//!    changing a single caller.
//! 2. **Limits are checked before the bytes are in memory**, never after. A cap
//!    enforced on a `Vec` that has already been filled is not a cap.
//! 3. **All network bytes are untrusted.** Nothing here interprets them; the most
//!    it will do is decode them to text under a charset the caller can inspect.

pub mod limits;
pub mod loader;
pub mod mime;
pub mod url;

pub use limits::Limits;
pub use loader::{Body, LoadRequest, LoadedResource, Loader, NetError};
pub use mime::{Sniffed, sniff};
pub use url::{is_fetchable, may_navigate, normalize, read_data_url, resolve};

/// Install the process-wide rustls crypto provider.
///
/// Call this once, early, from `main`. `rustls` picks a provider implicitly only
/// when exactly one is reachable in the build graph; the moment a second one
/// appears — through any dependency, at any depth — the choice becomes ambiguous
/// and building a `ClientConfig` panics instead of failing. Naming ours makes that
/// a decision rather than an accident.
///
/// Returns `false` if a provider was already installed, which is not an error.
pub fn install_crypto_provider() -> bool {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_ok()
}
