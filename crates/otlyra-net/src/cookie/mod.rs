//! Cookies: the state a site keeps in the browser.
//!
//! Two headers and a pile of rules. `Set-Cookie` is a server asking the browser
//! to remember a string; `Cookie` is the browser repeating it. Nothing in either
//! needs a script engine, which is why this is reachable now — and a browser
//! without them cannot stay signed in to anything, which is why it is worth
//! reaching.
//!
//! The work is not the parsing. It is the answer to *who may read what*, and
//! every rule in it is a near-miss away from being a vulnerability:
//!
//! - a domain match is **not** a suffix match, or `evil-example.com` reads
//!   `example.com`'s session;
//! - a path match is **not** a prefix match, or `/foobar` reads `/foo`'s;
//! - a name a registry hands out is a name nobody may write a cookie for, or one
//!   site under `.co.uk` writes a cookie every site under `.co.uk` sends back.
//!
//! The last of those is not derivable from a name — `com` is a registry, `co.uk`
//! is a registry, `co.com` is somebody's domain — so it is a list, and the list
//! is [vendored][suffix].
//!
//! ## The shape
//!
//! - [`SetCookie`] reads one header value and knows nothing else: no clock, no
//!   request, no jar.
//! - [`Jar`] decides whether the site that sent it was entitled to say it, keeps
//!   what survives, and answers which cookies go on a request.
//! - [`suffix`] answers where a registry ends and a registrant begins.
//! - [`date`] reads `Expires` the lenient way the specification asks for, because
//!   what servers send is not the format they are supposed to send.
//!
//! ## Invariants
//!
//! 1. **The clock is a parameter.** Nothing here reads `SystemTime::now()`. One
//!    request is decided against one instant, and a test states the instant
//!    rather than sleeping toward it.
//! 2. **A refusal is named.** Every rule that drops a cookie says which rule it
//!    was, because a person debugging their own site has to be able to see it.
//! 3. **A value is never decoded.** Percent-encoding, base64 and the rest are
//!    conventions between a site and itself. A browser that guessed would corrupt
//!    every cookie that is not encoded.

pub mod date;
pub mod jar;
pub mod parse;
pub mod store;
pub mod suffix;

pub use jar::{Capacity, Context, Cookie, Jar, MAX_LIFETIME, Refused, same_site};
pub use parse::{SameSite, SetCookie, Unreadable};
