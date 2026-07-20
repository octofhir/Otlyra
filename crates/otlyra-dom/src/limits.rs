//! What a hostile document is not allowed to cost us.

/// Caps on the shape of a parsed tree.
///
/// A document is untrusted input, and the tree builder will happily nest as deep as
/// the bytes tell it to. Every recursive walk we ever write — style, layout, paint,
/// serialization — then inherits that depth as stack depth. Bounding it once, here,
/// is cheaper than making every consumer defensive.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DomLimits {
    /// Deepest node we will insert, counting the document root as zero.
    pub max_depth: usize,

    /// Most attributes we will keep on one element.
    pub max_attrs_per_element: usize,
}

impl DomLimits {
    /// The defaults, applied unless a caller says otherwise.
    pub const DEFAULT: Self = Self {
        max_depth: 512,
        max_attrs_per_element: 1024,
    };
}

impl Default for DomLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}
