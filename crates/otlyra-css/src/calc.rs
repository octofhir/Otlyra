//! A math function that still has a percentage in it.
//!
//! `calc(100% - 16px)`, `min(100%, 18rem)`, `clamp(12rem, 50%, 40rem)`: every
//! length and every `em` in them is settled by the cascade, and what is left is a
//! percentage of a size that only layout knows. That is not a pair of numbers —
//! `min()` of a percentage and a length is a different function of the basis on
//! each side of the point where the two cross — so the tree the engine parsed is
//! kept whole and evaluated against the basis when there is one, which is what
//! CSS Values 4 §10 says a math function's computed value is: the tree, with its
//! percentages left in it until layout can resolve them.
//!
//! The engine's type stays behind the private field. Layout and paint ask it one
//! question — what it comes to against a basis — and never see what it is made of,
//! which is what keeps this crate the only one that speaks the engine's language.

use std::fmt;
use std::sync::Arc;

use style::values::computed::length_percentage::CalcLengthPercentage;
use style_traits::{CssWriter, ToCss};

/// A `calc()`, `min()`, `max()` or `clamp()` whose value depends on a percentage.
///
/// The engine's calc node rather than its whole `<length-percentage>`: a length
/// and a bare percentage are [`crate::Length`]'s other two variants, so this one
/// can only ever hold the case that needs a tree. Shared rather than copied,
/// because a style is cloned wherever layout needs a variant of it — a cell under
/// collapsed borders, an inline box broken over two lines — and a clone of a
/// `calc()` is then a pointer rather than a tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Calc(Arc<CalcLengthPercentage>);

impl Calc {
    /// Keep the engine's computed tree.
    pub(crate) fn new(tree: &CalcLengthPercentage) -> Self {
        Self(Arc::new(tree.clone()))
    }

    /// What the expression comes to when a percentage is of `basis`, in CSS
    /// pixels.
    ///
    /// The engine evaluates it — `min()`, `max()`, `clamp()` and nesting included —
    /// and applies the range the property allows, so a `width` that would come out
    /// negative comes out as zero — the range checking CSS Values 4 §10 does at
    /// used-value time rather than at parse time.
    pub fn resolve(&self, basis: f32) -> f32 {
        self.0
            .resolve(style::values::computed::Length::new(basis))
            .px()
    }
}

impl fmt::Display for Calc {
    /// The expression as a stylesheet would write it, which is what an inspector
    /// shows.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.to_css(&mut CssWriter::new(formatter))
    }
}
