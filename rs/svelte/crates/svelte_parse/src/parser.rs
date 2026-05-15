//! The `Parser` struct and its low-level helpers.
//!
//! Ported from `packages/svelte/src/compiler/phases/1-parse/index.js` — the
//! `Parser` class (lines 36-318). Field names and helper semantics match the
//! upstream behavior. Where Rust diverges from JS (e.g. recursive descent
//! instead of a `state, stack, fragments` triple), the divergence is noted.

use svelte_diagnostics::CompileDiagnostic;

use crate::utils::whitespace::is_whitespace;

/// Outcome from a parser helper: either it advanced the cursor and produced
/// a value, or it produced a fatal diagnostic.
pub type ParseResult<T> = Result<T, CompileDiagnostic>;

pub struct Parser<'src> {
    /// The template source after `trim_end` (matches upstream
    /// `index.js:95`: `this.template = template.trimEnd()`).
    pub template: &'src str,
    /// Current byte offset into `template`.
    pub index: usize,
    /// Whether the parser is in loose mode (returns partial AST on syntax
    /// errors rather than throwing).
    pub loose: bool,
    /// Whether `<script lang="ts">` was detected on entry. Mirrors
    /// `index.js:104`.
    pub ts: bool,
}

impl<'src> Parser<'src> {
    pub fn new(template: &'src str, loose: bool) -> Self {
        Self {
            template: template.trim_end(),
            index: 0,
            loose,
            ts: false,
        }
    }

    /// The original, untrimmed length matters for `Root.end` (`index.js:151`).
    /// We accept it as a separate input rather than re-discovering it.
    pub fn template_remaining(&self) -> &'src str {
        &self.template[self.index..]
    }

    /// Mirrors `Parser.match(str)` — does the source start with `s` at the
    /// current cursor?
    pub fn match_str(&self, s: &str) -> bool {
        self.template_remaining().starts_with(s)
    }

    /// Mirrors `Parser.eat(str, required, required_in_loose)`. Returns true
    /// if the cursor was advanced.
    ///
    /// The non-loose error path (when `required` is true) is deferred — until
    /// the diagnostic context is wired up, callers should `match_str` + custom
    /// error themselves.
    pub fn eat(&mut self, s: &str) -> bool {
        if self.match_str(s) {
            self.index += s.len();
            true
        } else {
            false
        }
    }

    /// Mirrors `Parser.allow_whitespace`.
    pub fn allow_whitespace(&mut self) {
        let bytes = self.template.as_bytes();
        while self.index < self.template.len() {
            // Fast path: ASCII whitespace via byte inspection.
            let b = bytes[self.index];
            if b < 0x80 {
                if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                    self.index += 1;
                    continue;
                }
                break;
            }
            // Non-ASCII: decode one char and check the full whitespace set.
            let ch = self.template[self.index..].chars().next().unwrap();
            if is_whitespace(ch) {
                self.index += ch.len_utf8();
            } else {
                break;
            }
        }
    }

    /// Strip a UTF-8 BOM if present at the very start (`compiler/index.js:183`).
    pub fn strip_bom(input: &str) -> &str {
        input.strip_prefix('\u{feff}').unwrap_or(input)
    }
}
