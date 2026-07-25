//! # otlyra-css — computed values, and the UA stylesheet
//!
//! ## Purpose
//!
//! What an element's style *is*, once every question has been answered. Today the
//! only thing answering them is a hardcoded user-agent table; at M8 Stylo answers
//! them from real stylesheets, and [`ComputedStyle`] is what it will fill in.
//!
//! ## Contents
//!
//! - [`style`] — [`ComputedStyle`] and the value types it is made of.
//! - [`ua`] — the user-agent stylesheet, both as CSS and as the table it replaces.
//! - [`appearance`] — `appearance`, carried through a cascade that lacks it.
//! - [`cascade`] — parsing stylesheets and computing a style per element.
//! - [`state`] — the state bits `:hover`, `:checked` and their kin are matched on.
//! - [`invalidation`] — whether a change of state can change anything at all.
//!
//! ## Invariants
//!
//! 1. **Computed values only.** Nothing here is a specified value, a token, or a
//!    string awaiting interpretation. `em` is already pixels; percentages are the
//!    one exception CSS itself defers to layout.
//! 2. **Exactly the properties this milestone needs.** Each one is a promise the
//!    box tree, layout and paint all have to keep.
//! 3. **No DOM, no layout, no painting.** This crate is values; who has them is the
//!    DOM's business and what they mean geometrically is layout's.

pub mod appearance;
pub mod cascade;
pub mod computed;
pub mod invalidation;
pub mod state;
pub mod style;
pub mod stylo_dom;
pub mod ua;

pub use style::{
    AlignContent, AlignItems, Anchor, BackgroundLayer, BackgroundPosition, BackgroundRepeat,
    BackgroundSize, Border, BorderCollapse, BorderStyle, BoxSizing, Clear, ComputedStyle, Corners,
    Display, FlexDirection, FlexWrap, Float, FontStyle, Gradient, GradientStop, JustifyContent,
    Length, LengthOrAuto, LineHeight, ListStyle, ObjectFit, Overflow, Position, Repeat, Shadow,
    Sides, TextAlign, TextDecoration, TextWrap, Track, TransformOp, TransformOrigin, VerticalAlign,
    WhiteSpace,
};
pub use ua::{has_renderable_children, initial_style, ua_style};
