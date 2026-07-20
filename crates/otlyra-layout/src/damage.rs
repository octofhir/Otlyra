//! What has to be redone, as a lattice rather than a set of flags.
//!
//! Four dirty booleans is the obvious design and the wrong one: every consumer ends
//! up asking "is style dirty *or* layout dirty *or*…", and each such question is a
//! place to forget a term. Servo's answer, which this copies, is to make the levels
//! a strict inclusion chain expressed as bitmasks, so that "does this need layout"
//! is one AND and cannot be got subtly wrong.
//!
//! The order is fixed by what each step depends on: restyling implies relayout,
//! relayout implies repainting, repainting implies compositing. Nothing needs the
//! reverse.

/// How much of the pipeline a change invalidates.
///
/// Each level *contains* the cheaper ones, which is what makes
/// [`Damage::contains`] a single bit test rather than a chain of comparisons.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Damage(u8);

impl Damage {
    /// Nothing to do.
    pub const NONE: Self = Self(0);
    /// The frame must be presented again — the same pixels, a new surface.
    pub const COMPOSITE: Self = Self(0b0001);
    /// The display list must be rebuilt. Implies compositing.
    pub const PAINT: Self = Self(0b0011);
    /// Layout must run again. Implies painting.
    pub const LAYOUT: Self = Self(0b0111);
    /// Style must be recomputed, and the box tree rebuilt. Implies layout.
    pub const STYLE: Self = Self(0b1111);

    /// Whether this damage includes `level`.
    pub fn contains(self, level: Self) -> bool {
        self.0 & level.0 == level.0
    }

    /// The worse of two damages — which, because the levels nest, is their union.
    pub fn max(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Record that something happened.
    pub fn add(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Take the damage and reset to none, as a frame does when it starts.
    pub fn take(&mut self) -> Self {
        std::mem::replace(self, Self::NONE)
    }

    /// Whether anything at all needs doing.
    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The damage a given reason causes.
    pub fn of(reason: crate::InvalidationReason) -> Self {
        use crate::InvalidationReason as Reason;
        match reason {
            // A new document is everything.
            Reason::DocumentLoaded => Self::STYLE,
            // The box tree does not depend on the viewport; the layout does.
            Reason::ViewportResized => Self::LAYOUT,
            Reason::NodeInserted | Reason::NodeRemoved | Reason::AttributeChanged => Self::STYLE,
            Reason::TextChanged => Self::LAYOUT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InvalidationReason;

    #[test]
    fn each_level_contains_the_cheaper_ones() {
        assert!(Damage::STYLE.contains(Damage::LAYOUT));
        assert!(Damage::STYLE.contains(Damage::PAINT));
        assert!(Damage::STYLE.contains(Damage::COMPOSITE));
        assert!(Damage::LAYOUT.contains(Damage::PAINT));
        assert!(Damage::PAINT.contains(Damage::COMPOSITE));
    }

    #[test]
    fn a_cheap_level_does_not_imply_an_expensive_one() {
        assert!(!Damage::PAINT.contains(Damage::LAYOUT));
        assert!(!Damage::COMPOSITE.contains(Damage::PAINT));
        assert!(!Damage::NONE.contains(Damage::COMPOSITE));
    }

    #[test]
    fn accumulating_damage_keeps_the_worst_of_it() {
        let mut damage = Damage::NONE;
        damage.add(Damage::PAINT);
        damage.add(Damage::STYLE);
        damage.add(Damage::COMPOSITE);
        assert_eq!(damage, Damage::STYLE);
    }

    #[test]
    fn taking_the_damage_clears_it() {
        let mut damage = Damage::LAYOUT;
        assert_eq!(damage.take(), Damage::LAYOUT);
        assert!(damage.is_none());
    }

    /// A resize does not change what boxes exist, only where they go. Rebuilding
    /// the box tree for a window drag would be work nobody asked for.
    #[test]
    fn a_resize_needs_layout_but_not_style() {
        let damage = Damage::of(InvalidationReason::ViewportResized);
        assert!(damage.contains(Damage::LAYOUT));
        assert!(!damage.contains(Damage::STYLE));
    }

    #[test]
    fn a_new_document_needs_everything() {
        assert_eq!(
            Damage::of(InvalidationReason::DocumentLoaded),
            Damage::STYLE
        );
    }
}
