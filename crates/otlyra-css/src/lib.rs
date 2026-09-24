//! # otlyra-css — computed values, and the UA stylesheet
//!
//! ## Purpose
//!
//! What an element's style *is*, once every question has been answered. Stylo
//! answers them, from the user-agent stylesheet and the page's own, and
//! [`ComputedStyle`] is what layout is handed of the answer.
//!
//! ## Contents
//!
//! - [`style`] — [`ComputedStyle`] and the value types it is made of.
//! - [`calc`] — a math function with a percentage in it, kept for layout.
//! - [`appearance`] — `appearance`, carried through a cascade that lacks it.
//! - [`cascade`] — parsing stylesheets, ours ([`cascade::UA_STYLESHEET`]) and the
//!   page's, and computing a style per element.
//! - [`computed`] — the engine's computed values, as a [`ComputedStyle`].
//! - [`state`] — the state bits `:hover`, `:checked` and their kin are matched on.
//! - [`invalidation`] — whether a change of state can change anything at all.
//!
//! ## Invariants
//!
//! 1. **Computed values only.** Nothing here is a specified value, a token, or a
//!    string awaiting interpretation. `em` is already pixels; percentages — bare,
//!    or inside a `calc()` — are the one exception CSS itself defers to layout,
//!    and layout resolves them without seeing the style engine's types.
//! 2. **Exactly the properties this milestone needs.** Each one is a promise the
//!    box tree, layout and paint all have to keep.
//! 3. **No DOM, no layout, no painting.** This crate is values; who has them is the
//!    DOM's business and what they mean geometrically is layout's.

pub mod appearance;
pub mod calc;
pub mod cascade;
pub mod computed;
pub mod invalidation;
pub mod state;
pub mod style;
pub mod stylo_dom;

pub use style::{
    AlignContent, AlignItems, AspectRatio, BackgroundLayer, BackgroundPosition, BackgroundRepeat,
    BackgroundSize, Border, BorderCollapse, BorderStyle, BoxSizing, Calc, Clear, ComputedStyle,
    Corners, Display, FlexBasis, FlexDirection, FlexWrap, Float, FontStyle, Gradient, GradientStop,
    Intrinsic, JustifyContent, Length, LengthOrAuto, LineHeight, ListStyle, MaxSize, ObjectFit,
    Overflow, Position, Ratio, Repeat, Shadow, Sides, Size, TextAlign, TextDecoration, TextWrap,
    Track, TransformOp, TransformOrigin, VerticalAlign, WhiteSpace,
};
