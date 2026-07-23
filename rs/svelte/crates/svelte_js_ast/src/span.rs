//! Source span for AST nodes.

/// `(start, end)` byte offsets into the original `.svelte` source. Default
/// is `(0, 0)`, used for transform-synthesized nodes that have no
/// underlying source range.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const ZERO: Span = Span { start: 0, end: 0 };

    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub const fn is_zero(self) -> bool {
        self.start == 0 && self.end == 0
    }
}

impl From<(u32, u32)> for Span {
    fn from((s, e): (u32, u32)) -> Self {
        Span::new(s, e)
    }
}
