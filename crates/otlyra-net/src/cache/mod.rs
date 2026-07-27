//! The HTTP cache: what has already been fetched, and whether it still counts.
//!
//! A browser without one refetches every stylesheet, every picture and every page
//! it has already seen — including going back to a page nothing has changed
//! about. The servers already say how long their answers are good for; the whole
//! of this is reading what they said and doing it.
//!
//! ## The shape
//!
//! - [`policy`] answers the three questions, and nothing else: may this be kept,
//!   how long is it good for, and what may be done with it now. Pure, and against
//!   a clock the caller passes in.
//! - [`store`] is what has been kept: one entry per address, `Vary` recorded
//!   against the request it was stored for, and a capacity that evicts what was
//!   used longest ago.
//!
//! ## Invariants
//!
//! 1. **Nothing here decides for itself how long something is good for**, except
//!    where a server said nothing at all — and then the guess is the
//!    specification's and is capped.
//! 2. **The clock is a parameter**, the same rule the cookie jar follows: one
//!    request is judged against one instant.
//! 3. **This is a private cache**, one reader's own, which is what makes
//!    `private` storable and `s-maxage` none of its business.

pub mod disk;
pub mod policy;
pub mod store;

pub use disk::Disk;
pub use policy::{Directives, Lifetime, Times, Use, lifetime, may_store, use_of};
pub use store::{Cache, Capacity, Stored};
