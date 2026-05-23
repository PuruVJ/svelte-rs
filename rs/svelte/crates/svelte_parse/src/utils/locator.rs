//! Byte offset → `(line, column)` lookup.
//!
//! Mirrors the role of `locate-character` in upstream, but tailored to what the
//! Svelte AST needs (`name_loc.character` is the byte offset, `loc.{start,end}`
//! carries line+column).
//!
//! Caveat: `column` here is a byte offset within the line, not a UTF-16
//! code-unit count or a Unicode codepoint count. For ASCII sources (the
//! common case for parser-modern fixtures) the three are identical. Non-ASCII
//! sources may yield columns that differ from acorn's output — to be fixed
//! in a follow-up.

use std::cell::Cell;

use svelte_ast::Position;

pub struct LineMap {
    /// Byte offsets at which each line begins. `line_starts[0]` is always 0.
    line_starts: Vec<usize>,
    /// Hint for monotonic forward scans (parser walks source left-to-right).
    hint_line: Cell<usize>,
}

impl LineMap {
    pub fn new(source: &str) -> Self {
        let mut starts = Vec::with_capacity(source.len() / 40 + 1);
        starts.push(0);
        for (i, b) in source.as_bytes().iter().enumerate() {
            if *b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self {
            line_starts: starts,
            hint_line: Cell::new(0),
        }
    }

    /// Locate a byte offset. Returns `(line, column)` with 1-based line and
    /// 0-based column — matches acorn's `loc` convention.
    pub fn locate(&self, offset: usize) -> (u32, u32) {
        let line = self.line_index(offset);
        let col = offset.saturating_sub(self.line_starts[line]);
        (line as u32 + 1, col as u32)
    }

    fn line_index(&self, offset: usize) -> usize {
        let mut line = self.hint_line.get();
        if offset < self.line_starts[line] {
            line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        } else {
            while line + 1 < self.line_starts.len() && self.line_starts[line + 1] <= offset {
                line += 1;
            }
        }
        self.hint_line.set(line);
        line
    }

    /// Build a full `Position` including `character` (= byte offset).
    pub fn position(&self, offset: usize) -> Position {
        let (line, column) = self.locate(offset);
        Position {
            line,
            column,
            character: Some(offset as u32),
        }
    }

    /// Build a `Position` *without* the `character` field. Used for `content.loc`
    /// inside `<script>` blocks, which mirrors acorn's `loc` (no character).
    pub fn position_no_char(&self, offset: usize) -> Position {
        let (line, column) = self.locate(offset);
        Position {
            line,
            column,
            character: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line() {
        let m = LineMap::new("hello world");
        assert_eq!(m.locate(0), (1, 0));
        assert_eq!(m.locate(5), (1, 5));
        assert_eq!(m.locate(11), (1, 11));
    }

    #[test]
    fn multi_line() {
        let m = LineMap::new("a\nb\nc");
        assert_eq!(m.locate(0), (1, 0));
        assert_eq!(m.locate(1), (1, 1)); // the '\n'
        assert_eq!(m.locate(2), (2, 0));
        assert_eq!(m.locate(3), (2, 1));
        assert_eq!(m.locate(4), (3, 0));
    }

    #[test]
    fn position_with_character() {
        let m = LineMap::new("ab\ncd");
        let p = m.position(4);
        assert_eq!(p.line, 2);
        assert_eq!(p.column, 1);
        assert_eq!(p.character, Some(4));
    }
}
