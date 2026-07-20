//! The caps a response is measured against.
//!
//! These exist because the far end of the connection is hostile until proven
//! otherwise. A server that never closes the body, or claims a gigabyte, or
//! redirects forever, must cost us a bounded amount of memory and time.

use std::time::Duration;

/// Resource limits applied to a single load.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest response body we will hold, in bytes.
    ///
    /// Checked against `Content-Length` before the first byte is read, and again
    /// against the running total as the body streams in — a server may lie about
    /// the length, or omit it entirely.
    pub max_body_bytes: u64,

    /// Largest redirect chain we will follow before giving up.
    pub max_redirects: usize,

    /// Wall-clock budget for the whole request, connection through last byte.
    pub timeout: Duration,
}

impl Limits {
    /// The limits for a top-level document load.
    pub const DOCUMENT: Self = Self {
        max_body_bytes: 32 * 1024 * 1024,
        max_redirects: 20,
        timeout: Duration::from_secs(30),
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DOCUMENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_limits_are_the_documented_numbers() {
        let limits = Limits::default();
        assert_eq!(limits.max_body_bytes, 33_554_432);
        assert_eq!(limits.max_redirects, 20);
        assert_eq!(limits.timeout, Duration::from_secs(30));
    }
}
