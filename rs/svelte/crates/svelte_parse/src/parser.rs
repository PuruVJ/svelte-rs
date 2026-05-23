//! The `Parser` struct and its low-level helpers.
//!
//! Ported from `packages/svelte/src/compiler/phases/1-parse/index.js` — the
//! `Parser` class (lines 36-318). Field names and helper semantics match the
//! upstream behavior. Where Rust diverges from JS (e.g. recursive descent
//! instead of a `state, stack, fragments` triple), the divergence is noted.

use oxc_allocator::Allocator;
use svelte_diagnostics::CompileDiagnostic;
use svelte_js_ast::Expression;

use crate::oxc_bridge::{parse_expression_at_with_comments, RawComment};
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
    /// Comments accumulated from inside `{expression}` / `<script>` parsing.
    /// Used to populate `Root.comments` at the end of `parse`. Mirrors the
    /// upstream `parser.root.comments` field.
    pub comments: Vec<RawComment>,
    /// Depth of `<template shadowrootmode="...">` ancestors at the current
    /// position. When `>0`, `<slot>` inside the current element is parsed as
    /// a `RegularElement` (real DOM slot), not Svelte's `SlotElement`. See
    /// `parent_is_shadowroot_template` in `element.js:455-468`.
    pub shadowroot_depth: u32,
    /// Soft diagnostics accumulated during parsing. Surface alongside the
    /// final Root so analyze can treat them as warnings (e.g.
    /// `element_invalid_self_closing_tag`, `element_implicitly_closed`).
    pub warnings: Vec<CompileDiagnostic>,
    /// Name of the tag that was most recently auto-closed by HTML
    /// implicit-close rules (e.g. `<p>` closed by `<pre>`). Cleared once
    /// the next element finishes. Used to emit
    /// `element_invalid_closing_tag_autoclosed` when a stray `</p>` shows
    /// up immediately after.
    pub last_auto_closed_tag: Option<LastAutoClosed>,
    /// Current nesting depth of regular elements being parsed. Used so a
    /// surfaced `last_auto_closed_tag` can be cleared once we've popped
    /// past the element it was set inside of.
    pub element_depth: usize,
    /// Reused OXC bump allocator for JS/TS expression and script parsing.
    /// Reset after each OXC parse once the typed AST is materialized.
    pub oxc_alloc: Allocator,
}

#[derive(Debug, Clone)]
pub struct LastAutoClosed {
    pub tag: String,
    pub closer: String,
    /// Element-depth at which the auto-close happened. When the parser's
    /// stack drops below this depth (i.e. we've popped past the
    /// surrounding element), the auto-close info should be forgotten.
    /// Mirrors upstream's `parser.last_auto_closed_tag.depth` check at
    /// element.js:133-134.
    pub depth: usize,
}

impl<'src> Parser<'src> {
    pub fn new(template: &'src str, loose: bool) -> Self {
        let template = template.trim_end();
        let line_map = LineMap::new(template);
        let ts = detect_typescript(template);
        Self {
            template,
            index: 0,
            loose,
            ts,
            line_map,
            comments: Vec::with_capacity(4),
            shadowroot_depth: 0,
            warnings: Vec::with_capacity(4),
            last_auto_closed_tag: None,
            element_depth: 0,
            oxc_alloc: Allocator::default(),
        }
    }

    /// Parse a JS/TS expression starting at `start`, accumulating any
    /// comments OXC saw into `self.comments`. Returns the estree-shaped
    /// JSON for the expression and the absolute end offset.
    pub fn parse_expression_at(
        &mut self,
        start: usize,
    ) -> Result<(Expression, usize), CompileDiagnostic> {
        let (expr, end, comments) = parse_expression_at_with_comments(
            &mut self.oxc_alloc,
            self.template,
            &self.line_map,
            start,
            self.ts,
        )?;
        self.comments.extend(comments);
        Ok((expr, end))
    }

    /// Parse the body of `{@const ...}` as a VariableDeclaration.
    pub fn parse_const_decl_at(
        &mut self,
        start: usize,
        end: usize,
    ) -> Result<svelte_js_ast::VariableDeclaration, CompileDiagnostic> {
        crate::oxc_bridge::parse_const_decl_at(
            &mut self.oxc_alloc,
            self.template,
            &self.line_map,
            start,
            end,
            self.ts,
        )
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

    /// Skip whitespace and JS-style comments (`//` and `/* */`). Used inside
    /// mustache expressions where trailing comments may follow the expression
    /// before the closing `}`. Collected comments are pushed onto
    /// `self.comments` so they end up in `Root.comments`.
    pub fn skip_whitespace_and_js_comments(&mut self) {
        let bytes_len = self.template.len();
        loop {
            self.allow_whitespace();
            if self.index + 2 > bytes_len {
                return;
            }
            let b0 = self.template.as_bytes()[self.index];
            let b1 = self.template.as_bytes()[self.index + 1];
            if b0 == b'/' && b1 == b'/' {
                let start = self.index;
                let mut j = self.index + 2;
                while j < bytes_len && self.template.as_bytes()[j] != b'\n' {
                    j += 1;
                }
                let value = self.template[start + 2..j].into();
                self.index = j;
                self.comments.push(crate::oxc_bridge::RawComment {
                    line: true,
                    start: start as u32,
                    end: j as u32,
                    value,
                    with_character: false,
                });
                continue;
            }
            if b0 == b'/' && b1 == b'*' {
                let start = self.index;
                let mut j = self.index + 2;
                while j + 1 < bytes_len
                    && !(self.template.as_bytes()[j] == b'*'
                        && self.template.as_bytes()[j + 1] == b'/')
                {
                    j += 1;
                }
                let value_end = j;
                if j + 1 < bytes_len {
                    j += 2;
                }
                let value = self.template[start + 2..value_end].into();
                self.index = j;
                self.comments.push(crate::oxc_bridge::RawComment {
                    line: false,
                    start: start as u32,
                    end: j as u32,
                    value,
                    with_character: false,
                });
                continue;
            }
            return;
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

/// Detect a `lang="ts"` (or `lang="typescript"`) attribute on a `<script>` tag.
///
/// Mirrors `phases/1-parse/index.js:33-34`. Upstream uses a regex that
/// ignores HTML comments and walks through arbitrary attributes; we do a
/// simpler linear scan that's correct for valid documents.
///
/// Bytewise scan — markers (`<!--`, `<script`, `-->`, `>`) are all ASCII so
/// we never need to land on multibyte char boundaries.
fn detect_typescript(template: &str) -> bool {
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if starts_with(bytes, i, b"<!--") {
            match find_subslice(bytes, i + 4, b"-->") {
                Some(end) => i = end + 3,
                None => return false,
            }
            continue;
        }
        if starts_with(bytes, i, b"<script") {
            // Match a closing `>` (skipping anything in between).
            let tag_start = i;
            let Some(tag_end) = find_subslice(bytes, tag_start, b">") else {
                return false;
            };
            // Search for `lang=` inside the open tag.
            if let Some(lang_pos) = find_subslice(&bytes[tag_start..tag_end], 0, b"lang=") {
                let after = &bytes[tag_start + lang_pos + 5..tag_end];
                if lang_matches(after, b"ts") || lang_matches(after, b"typescript") {
                    return true;
                }
            }
            i = tag_end + 1;
            continue;
        }
        i += 1;
    }
    false
}

fn starts_with(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    haystack.len() >= at + needle.len() && &haystack[at..at + needle.len()] == needle
}

fn find_subslice(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from + needle.len() > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

fn lang_matches(after_eq: &[u8], wanted: &[u8]) -> bool {
    if let Some(rest) = strip_byte_prefix(after_eq, b'"') {
        if let Some(end) = rest.iter().position(|&b| b == b'"') {
            return &rest[..end] == wanted;
        }
    }
    if let Some(rest) = strip_byte_prefix(after_eq, b'\'') {
        if let Some(end) = rest.iter().position(|&b| b == b'\'') {
            return &rest[..end] == wanted;
        }
    }
    // Unquoted: terminator is whitespace or `>`.
    let end = after_eq
        .iter()
        .position(|&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'>'))
        .unwrap_or(after_eq.len());
    &after_eq[..end] == wanted
}

fn strip_byte_prefix(s: &[u8], b: u8) -> Option<&[u8]> {
    if s.first().copied() == Some(b) {
        Some(&s[1..])
    } else {
        None
    }
}

