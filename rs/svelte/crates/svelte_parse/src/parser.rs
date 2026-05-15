//! The `Parser` struct and its low-level helpers.
//!
//! Ported from `packages/svelte/src/compiler/phases/1-parse/index.js` — the
//! `Parser` class (lines 36-318). Field names and helper semantics match the
//! upstream behavior. Where Rust diverges from JS (e.g. recursive descent
//! instead of a `state, stack, fragments` triple), the divergence is noted.

use svelte_diagnostics::CompileDiagnostic;

use crate::utils::locator::LineMap;
use crate::utils::whitespace::is_whitespace;

pub type ParseResult<T> = Result<T, CompileDiagnostic>;

pub struct Parser<'src> {
    /// The template source after `trim_end` (matches upstream
    /// `index.js:95`: `this.template = template.trimEnd()`).
    pub template: &'src str,
    pub index: usize,
    pub loose: bool,
    pub ts: bool,
    /// Maps byte offsets to line+column for `name_loc` / `loc` fields.
    /// Built once when the Parser is constructed; cheap to query.
    pub line_map: LineMap,
}

impl<'src> Parser<'src> {
    pub fn new(template: &'src str, loose: bool) -> Self {
        let template = template.trim_end();
        let line_map = LineMap::new(template);
        Self {
            template,
            index: 0,
            loose,
            ts: false,
            line_map,
        }
    }

    pub fn template_remaining(&self) -> &'src str {
        &self.template[self.index..]
    }

    pub fn match_str(&self, s: &str) -> bool {
        self.template_remaining().starts_with(s)
    }

    /// Mirrors `Parser.eat`. Returns whether the cursor advanced.
    pub fn eat(&mut self, s: &str) -> bool {
        if self.match_str(s) {
            self.index += s.len();
            true
        } else {
            false
        }
    }

    pub fn allow_whitespace(&mut self) {
        let bytes = self.template.as_bytes();
        while self.index < self.template.len() {
            let b = bytes[self.index];
            if b < 0x80 {
                if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                    self.index += 1;
                    continue;
                }
                break;
            }
            let ch = self.template[self.index..].chars().next().unwrap();
            if is_whitespace(ch) {
                self.index += ch.len_utf8();
            } else {
                break;
            }
        }
    }

    /// Read while `pred(c)` returns true. Returns the slice consumed.
    pub fn read_while<F: Fn(u8) -> bool>(&mut self, pred: F) -> &'src str {
        let start = self.index;
        let bytes = self.template.as_bytes();
        while self.index < self.template.len() {
            let b = bytes[self.index];
            if !pred(b) {
                break;
            }
            self.index += 1;
        }
        &self.template[start..self.index]
    }

    pub fn strip_bom(input: &str) -> &str {
        input.strip_prefix('\u{feff}').unwrap_or(input)
    }

    /// Peek at the next byte without advancing. None at EOF.
    pub fn peek(&self) -> Option<u8> {
        self.template.as_bytes().get(self.index).copied()
    }
}
