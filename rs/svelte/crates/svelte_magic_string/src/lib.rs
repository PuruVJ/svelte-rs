//! Port of the `magic-string` npm package.
//!
//! Source of truth: `node_modules/magic-string@0.30.17/dist/magic-string.es.mjs`
//! and the way Svelte's compiler exercises it (under
//! `packages/svelte/src/compiler/phases/3-transform/` and `phases/migrate/`).
//!
//! The data model is a doubly-linked list of `Chunk`s, each tracking the
//! original `start..end` byte span plus `intro` (prepended), `content`
//! (replaced body), and `outro` (appended) strings. Edits split chunks on
//! demand so every edit boundary lands on a chunk boundary.
//!
//! We use arena indices (`ChunkId = usize`) into a `Vec<Chunk>` instead of
//! `Rc<RefCell<...>>`, which keeps the structure cache-friendly and lets
//! us model the linked list cleanly under Rust's aliasing rules. Removed
//! chunks are unlinked but their slots are not freed (mirrors upstream's
//! behavior — chunks are never reclaimed).

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

pub mod sourcemap;

pub use sourcemap::{encode_mappings, DecodedMap, Hires, Locator, Mappings, Segment};

pub type ChunkId = usize;

/// Options for [`MagicString::generate_decoded_map`] / [`generate_map`].
#[derive(Debug, Clone, Default)]
pub struct GenerateMapOptions {
    pub file: Option<String>,
    pub source: Option<String>,
    pub include_content: bool,
    pub hires: Hires,
}

impl Default for Hires {
    fn default() -> Self {
        Hires::Off
    }
}

/// VLQ-encoded sourcemap output suitable for emitting as a JSON `.map` file.
#[derive(Debug, Clone)]
pub struct EncodedMap {
    pub file: Option<String>,
    pub sources: Vec<String>,
    pub sources_content: Option<Vec<String>>,
    pub names: Vec<String>,
    pub mappings: String,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub intro: String,
    pub outro: String,
    pub content: String,
    pub store_name: bool,
    pub edited: bool,
    pub previous: Option<ChunkId>,
    pub next: Option<ChunkId>,
}

impl Chunk {
    fn new(start: usize, end: usize, content: String) -> Self {
        Self {
            start,
            end,
            original: content.clone(),
            intro: String::new(),
            outro: String::new(),
            content,
            store_name: false,
            edited: false,
            previous: None,
            next: None,
        }
    }

    fn edit(&mut self, content: String, store_name: bool) -> &mut Self {
        self.content = content;
        self.intro.clear();
        self.outro.clear();
        self.store_name = store_name;
        self.edited = true;
        self
    }
}

#[derive(Debug)]
pub struct MagicString {
    pub original: String,
    chunks: Vec<Chunk>,
    pub first_chunk: ChunkId,
    pub last_chunk: ChunkId,
    pub intro: String,
    pub outro: String,
    by_start: HashMap<usize, ChunkId>,
    by_end: HashMap<usize, ChunkId>,
    pub offset: usize,
    /// Forced segment locations — populated via `add_sourcemap_location(idx)`.
    /// Mirrors upstream's `Set` field, used by `addUneditedChunk` to emit a
    /// mapping at indices that wouldn't otherwise produce one (line starts /
    /// hires boundaries are automatic; these are extra).
    pub sourcemap_locations: HashSet<usize>,
}

impl MagicString {
    pub fn new(source: impl Into<String>) -> Self {
        let original: String = source.into();
        let first = Chunk::new(0, original.len(), original.clone());
        let mut chunks = Vec::with_capacity(64);
        chunks.push(first);
        let mut by_start = HashMap::new();
        let mut by_end = HashMap::new();
        by_start.insert(0, 0);
        by_end.insert(original.len(), 0);
        Self {
            original,
            chunks,
            first_chunk: 0,
            last_chunk: 0,
            intro: String::new(),
            outro: String::new(),
            by_start,
            by_end,
            offset: 0,
            sourcemap_locations: HashSet::new(),
        }
    }

    /// Mark an original-source index as a forced sourcemap segment location.
    /// Mirrors `MagicString.addSourcemapLocation`.
    pub fn add_sourcemap_location(&mut self, idx: usize) -> &mut Self {
        self.sourcemap_locations.insert(idx);
        self
    }

    /// Emit a decoded sourcemap for the current state of this MagicString.
    /// Mirrors `MagicString.generateDecodedMap` (`magic-string.es.mjs:542-584`).
    pub fn generate_decoded_map(&self, options: GenerateMapOptions) -> DecodedMap {
        let mut mappings = Mappings::new(options.hires);
        let locator = Locator::new(&self.original);
        let source_index = 0u32;

        if !self.intro.is_empty() {
            mappings.advance(&self.intro);
        }

        // Walk the chunk linked list from first to last.
        let mut current = Some(self.first_chunk);
        while let Some(id) = current {
            let chunk = &self.chunks[id];
            let loc = locator.locate(chunk.start);
            if !chunk.intro.is_empty() {
                mappings.advance(&chunk.intro);
            }
            if chunk.edited {
                mappings.add_edit(source_index, &chunk.content, loc, None);
            } else {
                mappings.add_unedited_chunk(
                    source_index,
                    chunk.start,
                    chunk.end,
                    &self.original,
                    loc,
                    &self.sourcemap_locations,
                );
            }
            if !chunk.outro.is_empty() {
                mappings.advance(&chunk.outro);
            }
            current = chunk.next;
        }

        DecodedMap {
            file: options.file.clone(),
            sources: vec![options.source.clone().unwrap_or_default()],
            sources_content: if options.include_content {
                Some(vec![self.original.clone()])
            } else {
                None
            },
            names: Vec::new(),
            mappings: mappings.raw,
        }
    }

    /// Convenience: encode the decoded map to VLQ form and return both pieces.
    pub fn generate_map(&self, options: GenerateMapOptions) -> EncodedMap {
        let decoded = self.generate_decoded_map(options);
        let mappings = encode_mappings(&decoded.mappings);
        EncodedMap {
            file: decoded.file,
            sources: decoded.sources,
            sources_content: decoded.sources_content,
            names: decoded.names,
            mappings,
        }
    }

    fn chunk(&self, id: ChunkId) -> &Chunk {
        &self.chunks[id]
    }
    fn chunk_mut(&mut self, id: ChunkId) -> &mut Chunk {
        &mut self.chunks[id]
    }

    /// Append `content` to the end of the output. Mirrors `MagicString.append`.
    pub fn append(&mut self, content: impl AsRef<str>) -> &mut Self {
        self.outro.push_str(content.as_ref());
        self
    }

    /// Prepend `content` before everything in the output. Mirrors
    /// `MagicString.prepend`.
    pub fn prepend(&mut self, content: impl AsRef<str>) -> &mut Self {
        let s = content.as_ref();
        if !s.is_empty() {
            self.intro = format!("{}{}", s, self.intro);
        }
        self
    }

    /// Insert `content` at `index` grouped with the chunk LEFT of the
    /// boundary (so a later `remove` of the right span keeps it).
    /// Mirrors `MagicString.appendLeft`.
    pub fn append_left(&mut self, index: usize, content: impl AsRef<str>) -> &mut Self {
        let index = index + self.offset;
        let content = content.as_ref();
        self._split(index);
        if let Some(&id) = self.by_end.get(&index) {
            self.chunk_mut(id).outro.push_str(content);
        } else {
            self.intro.push_str(content);
        }
        self
    }

    /// Insert `content` at `index` grouped with the chunk RIGHT of the
    /// boundary. Mirrors `MagicString.appendRight`.
    pub fn append_right(&mut self, index: usize, content: impl AsRef<str>) -> &mut Self {
        let index = index + self.offset;
        let content = content.as_ref();
        self._split(index);
        if let Some(&id) = self.by_start.get(&index) {
            self.chunk_mut(id).intro.push_str(content);
        } else {
            self.outro.push_str(content);
        }
        self
    }

    /// Insert `content` at `index` grouped with the chunk LEFT of the
    /// boundary (prepended). Mirrors `MagicString.prependLeft`.
    pub fn prepend_left(&mut self, index: usize, content: impl AsRef<str>) -> &mut Self {
        let index = index + self.offset;
        let content = content.as_ref();
        self._split(index);
        if let Some(&id) = self.by_end.get(&index) {
            let c = self.chunk_mut(id);
            c.outro = format!("{}{}", content, c.outro);
        } else {
            self.intro = format!("{}{}", content, self.intro);
        }
        self
    }

    /// Insert `content` at `index` grouped with the chunk RIGHT of the
    /// boundary (prepended). Mirrors `MagicString.prependRight`.
    pub fn prepend_right(&mut self, index: usize, content: impl AsRef<str>) -> &mut Self {
        let index = index + self.offset;
        let content = content.as_ref();
        self._split(index);
        if let Some(&id) = self.by_start.get(&index) {
            let c = self.chunk_mut(id);
            c.intro = format!("{}{}", content, c.intro);
        } else {
            self.outro = format!("{}{}", content, self.outro);
        }
        self
    }

    /// Move the chunks covering source range `[start, end)` so they appear
    /// immediately before the chunk starting at `index`. Mirrors
    /// `MagicString.move(start, end, index)`.
    pub fn move_range(&mut self, start: usize, end: usize, index: usize) -> &mut Self {
        let start = start + self.offset;
        let end = end + self.offset;
        let index = index + self.offset;
        if index >= start && index <= end {
            panic!("Cannot move a selection inside itself");
        }
        if start == end {
            return self;
        }
        self._split(start);
        self._split(end);
        self._split(index);

        let first = match self.by_start.get(&start).copied() {
            Some(id) => id,
            None => return self,
        };
        let last = match self.by_end.get(&end).copied() {
            Some(id) => id,
            None => return self,
        };

        let old_left = self.chunks[first].previous;
        let old_right = self.chunks[last].next;

        let new_right = self.by_start.get(&index).copied();
        if new_right.is_none() && last == self.last_chunk {
            return self;
        }
        let new_left = match new_right {
            Some(nr) => self.chunks[nr].previous,
            None => Some(self.last_chunk),
        };

        if let Some(ol) = old_left {
            self.chunks[ol].next = old_right;
        }
        if let Some(or) = old_right {
            self.chunks[or].previous = old_left;
        }
        if let Some(nl) = new_left {
            self.chunks[nl].next = Some(first);
        }
        if let Some(nr) = new_right {
            self.chunks[nr].previous = Some(last);
        }
        if self.chunks[first].previous.is_none() {
            // first was firstChunk — now first's old next becomes firstChunk
            self.first_chunk = old_right.unwrap_or(first);
        }
        if self.chunks[last].next.is_none() {
            // last was lastChunk — its predecessor becomes lastChunk
            self.last_chunk = old_left.unwrap_or(last);
            self.chunks[self.last_chunk].next = None;
        }

        self.chunks[first].previous = new_left;
        self.chunks[last].next = new_right;

        if new_left.is_none() {
            self.first_chunk = first;
        }
        if new_right.is_none() {
            self.last_chunk = last;
        }
        self
    }

    /// Remove `start..end`. Intro / outro on affected chunks are cleared.
    /// Mirrors `MagicString.remove`.
    pub fn remove(&mut self, start: usize, end: usize) -> &mut Self {
        let start = start + self.offset;
        let end = end + self.offset;
        if start == end {
            return self;
        }
        assert!(
            start <= self.original.len() && end <= self.original.len(),
            "Character out of bounds"
        );
        assert!(start <= end, "end must be >= start");
        self._split(start);
        self._split(end);
        let mut chunk = self.by_start.get(&start).copied();
        while let Some(id) = chunk {
            self.chunk_mut(id).intro.clear();
            self.chunk_mut(id).outro.clear();
            self.chunk_mut(id).content.clear();
            self.chunk_mut(id).edited = true;
            let cur_end = self.chunk(id).end;
            chunk = if end > cur_end {
                self.by_start.get(&cur_end).copied()
            } else {
                None
            };
        }
        self
    }

    /// Overwrite `start..end` with `content` (clears any prepend/append
    /// on affected chunks). Mirrors `MagicString.overwrite`.
    pub fn overwrite(
        &mut self,
        start: usize,
        end: usize,
        content: impl Into<String>,
    ) -> &mut Self {
        self.update_internal(start, end, content.into(), false, true)
    }

    /// Update `start..end` with `content` (preserves prepend/append).
    /// Mirrors `MagicString.update` with `overwrite: false`.
    pub fn update(
        &mut self,
        start: usize,
        end: usize,
        content: impl Into<String>,
    ) -> &mut Self {
        self.update_internal(start, end, content.into(), false, false)
    }

    fn update_internal(
        &mut self,
        start: usize,
        end: usize,
        content: String,
        store_name: bool,
        overwrite: bool,
    ) -> &mut Self {
        let start = start + self.offset;
        let end = end + self.offset;
        assert!(end <= self.original.len(), "end is out of bounds");
        assert!(
            start != end,
            "Cannot overwrite a zero-length range — use appendLeft or prependRight instead"
        );
        self._split(start);
        self._split(end);
        let first = self.by_start.get(&start).copied();
        let last = self.by_end.get(&end).copied();
        if let (Some(first), Some(last)) = (first, last) {
            let mut cur = first;
            while cur != last {
                let next = self.chunk(cur).next;
                let next_id = next.expect("chunk chain broken");
                let by_start_at_end = self.by_start.get(&self.chunk(cur).end).copied();
                if next != by_start_at_end {
                    panic!("Cannot overwrite across a split point");
                }
                cur = next_id;
                self.edit_chunk(cur, String::new(), false, false);
            }
            self.edit_chunk(first, content, store_name, !overwrite);
        } else {
            // Insert past the end — make a fresh chunk.
            let new_id = self.chunks.len();
            let mut nc = Chunk::new(start, end, String::new());
            nc.content = content;
            nc.edited = true;
            nc.store_name = store_name;
            nc.previous = last;
            self.chunks.push(nc);
            if let Some(l) = last {
                self.chunk_mut(l).next = Some(new_id);
            }
        }
        self
    }

    fn edit_chunk(&mut self, id: ChunkId, content: String, store_name: bool, content_only: bool) {
        let c = self.chunk_mut(id);
        c.content = content;
        if !content_only {
            c.intro.clear();
            c.outro.clear();
        }
        c.store_name = store_name;
        c.edited = true;
    }

    /// Render the full edited string. Mirrors `MagicString.toString`.
    pub fn to_string(&self) -> String {
        let mut out = String::with_capacity(self.original.len());
        out.push_str(&self.intro);
        let mut id = Some(self.first_chunk);
        while let Some(cid) = id {
            let c = self.chunk(cid);
            out.push_str(&c.intro);
            out.push_str(&c.content);
            out.push_str(&c.outro);
            id = c.next;
        }
        out.push_str(&self.outro);
        out
    }

    /// Slice the rendered string. Mirrors `MagicString.slice`. Errors if
    /// `start` or `end` lands mid-edit.
    pub fn slice(&self, start: usize, end: usize) -> String {
        let start = start + self.offset;
        let end = end + self.offset;
        // Find the chunk containing `start`.
        let mut cid = Some(self.first_chunk);
        loop {
            let Some(id) = cid else { break };
            let c = self.chunk(id);
            if c.start <= start && start < c.end {
                break;
            }
            if c.start < end && c.end >= end {
                return String::new();
            }
            cid = c.next;
        }
        let Some(start_chunk) = cid else {
            return String::new();
        };
        if self.chunk(start_chunk).edited && self.chunk(start_chunk).start != start {
            panic!("Cannot use replaced character {start} as slice start anchor.");
        }
        let mut result = String::new();
        let mut id_opt = Some(start_chunk);
        while let Some(id) = id_opt {
            let c = self.chunk(id);
            if id != start_chunk && !c.intro.is_empty() {
                result.push_str(&c.intro);
            }
            let contains_end_chunk = c.start < end && c.end >= end;
            if contains_end_chunk && c.edited && c.end != end {
                panic!("Cannot use replaced character {end} as slice end anchor.");
            }
            let slice_start = if id == start_chunk { start - c.start } else { 0 };
            let slice_end = if contains_end_chunk {
                // Only valid for unedited chunks since we panicked above.
                c.content.len() - (c.end - end)
            } else {
                c.content.len()
            };
            let lo = slice_start.min(c.content.len());
            let hi = slice_end.min(c.content.len());
            if hi > lo {
                result.push_str(&c.content[lo..hi]);
            }
            if !contains_end_chunk && !c.outro.is_empty() {
                result.push_str(&c.outro);
            }
            if contains_end_chunk {
                break;
            }
            id_opt = c.next;
        }
        result
    }

    /// Clone the `MagicString` including all edits. Mirrors
    /// `MagicString.clone`.
    pub fn clone_ms(&self) -> Self {
        let mut cloned = Self::new(self.original.clone());
        cloned.chunks.clear();
        cloned.by_start.clear();
        cloned.by_end.clear();
        let mut id_opt = Some(self.first_chunk);
        let mut new_prev: Option<ChunkId> = None;
        let mut new_first: Option<ChunkId> = None;
        let mut new_last: Option<ChunkId> = None;
        while let Some(id) = id_opt {
            let c = self.chunk(id).clone();
            let nid = cloned.chunks.len();
            cloned.chunks.push(Chunk {
                previous: new_prev,
                next: None,
                ..c
            });
            cloned.by_start.insert(cloned.chunks[nid].start, nid);
            cloned.by_end.insert(cloned.chunks[nid].end, nid);
            if let Some(p) = new_prev {
                cloned.chunks[p].next = Some(nid);
            }
            if new_first.is_none() {
                new_first = Some(nid);
            }
            new_last = Some(nid);
            new_prev = Some(nid);
            id_opt = self.chunk(id).next;
        }
        cloned.first_chunk = new_first.unwrap_or(0);
        cloned.last_chunk = new_last.unwrap_or(0);
        cloned.intro = self.intro.clone();
        cloned.outro = self.outro.clone();
        cloned.offset = self.offset;
        cloned
    }

    /// True if the rendered string is whitespace-only. Mirrors
    /// `MagicString.isEmpty`.
    pub fn is_empty(&self) -> bool {
        let mut id_opt = Some(self.first_chunk);
        while let Some(id) = id_opt {
            let c = self.chunk(id);
            if !c.intro.trim().is_empty()
                || !c.content.trim().is_empty()
                || !c.outro.trim().is_empty()
            {
                return false;
            }
            id_opt = c.next;
        }
        self.intro.trim().is_empty() && self.outro.trim().is_empty()
    }

    /// Total length of the rendered string. Mirrors `MagicString.length`.
    pub fn length(&self) -> usize {
        let mut total = self.intro.len() + self.outro.len();
        let mut id_opt = Some(self.first_chunk);
        while let Some(id) = id_opt {
            let c = self.chunk(id);
            total += c.intro.len() + c.content.len() + c.outro.len();
            id_opt = c.next;
        }
        total
    }

    /// True if any edit changed the rendered output. Mirrors
    /// `MagicString.hasChanged`.
    pub fn has_changed(&self) -> bool {
        self.original != self.to_string()
    }

    /// Replace the first occurrence of `needle` with `replacement`.
    /// Mirrors `MagicString.replace(string, string)`. (Regex form deferred —
    /// requires Rust regex support and the full $& / $1 substitution
    /// rules.)
    pub fn replace(&mut self, needle: &str, replacement: &str) -> &mut Self {
        if let Some(idx) = self.original.find(needle) {
            self.overwrite(idx, idx + needle.len(), replacement);
        }
        self
    }

    /// Replace every occurrence of `needle` with `replacement`. Mirrors
    /// `MagicString.replaceAll(string, string)`.
    pub fn replace_all(&mut self, needle: &str, replacement: &str) -> &mut Self {
        if needle.is_empty() {
            return self;
        }
        let mut idx = 0;
        let len = needle.len();
        let original = self.original.clone();
        while let Some(found) = original[idx..].find(needle) {
            let abs = idx + found;
            self.overwrite(abs, abs + len, replacement);
            idx = abs + len;
        }
        self
    }

    /// Indent every line of the output with `indent_str`. Mirrors
    /// `MagicString.indent(indentStr)` — the no-options form. Excluded
    /// ranges and the `indentStart: false` option are TODO.
    pub fn indent(&mut self, indent_str: &str) -> &mut Self {
        if indent_str.is_empty() {
            return self;
        }
        let mut should_indent = true;

        // Helper closure: scan string, prepend `indent_str` after each
        // newline.
        let replace = |s: &str, should_indent: &mut bool| -> String {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if c == '\n' {
                    out.push(c);
                    *should_indent = true;
                } else if c == '\r' {
                    out.push(c);
                } else {
                    if *should_indent {
                        out.push_str(indent_str);
                        *should_indent = false;
                    }
                    out.push(c);
                }
            }
            out
        };

        self.intro = replace(&self.intro, &mut should_indent);

        let mut id_opt = Some(self.first_chunk);
        while let Some(id) = id_opt {
            let edited = self.chunk(id).edited;
            if edited {
                let new_content = replace(&self.chunks[id].content, &mut should_indent);
                self.chunks[id].content = new_content;
            } else {
                // For unedited chunks, walk the original char-by-char and
                // prepend the indent into the right chunk's intro at
                // line-boundary positions.
                let start = self.chunk(id).start;
                let end = self.chunk(id).end;
                let mut char_index = start;
                while char_index < end {
                    let ch = self.original[char_index..]
                        .chars()
                        .next()
                        .unwrap();
                    if ch == '\n' {
                        should_indent = true;
                    } else if ch != '\r' && should_indent {
                        should_indent = false;
                        if char_index == start {
                            // prepend_right but on this chunk only
                            self.chunks[id].intro =
                                format!("{}{}", self.chunks[id].intro, indent_str);
                        } else {
                            let new_id = self.split_chunk(id, char_index);
                            self.chunks[new_id].intro =
                                format!("{}{}", self.chunks[new_id].intro, indent_str);
                            // Switch focus to new chunk and continue the
                            // walk from there.
                            id_opt = Some(new_id);
                            break;
                        }
                    }
                    char_index += ch.len_utf8();
                }
            }
            // Advance unless the inner break re-set id_opt.
            if let Some(cur) = id_opt {
                if cur == id {
                    id_opt = self.chunk(id).next;
                }
            }
        }
        self.outro = replace(&self.outro, &mut should_indent);
        self
    }

    /// Trim whitespace from both ends. Mirrors `MagicString.trim()`.
    pub fn trim(&mut self) -> &mut Self {
        self.trim_start();
        self.trim_end();
        self
    }

    /// Trim leading whitespace. Mirrors `MagicString.trimStart()` for the
    /// default (`\s`) char class.
    pub fn trim_start(&mut self) -> &mut Self {
        let stripped = self.intro.trim_start();
        if stripped.len() != self.intro.len() {
            self.intro = stripped.to_string();
        }
        if !self.intro.is_empty() {
            return self;
        }
        // Walk chunks left-to-right trimming the leading whitespace.
        let mut id_opt = Some(self.first_chunk);
        while let Some(id) = id_opt {
            if self.trim_chunk_start(id) {
                return self;
            }
            id_opt = self.chunk(id).next;
        }
        self
    }

    /// Trim trailing whitespace. Mirrors `MagicString.trimEnd()` for the
    /// default `\s` char class.
    pub fn trim_end(&mut self) -> &mut Self {
        let stripped = self.outro.trim_end();
        if stripped.len() != self.outro.len() {
            self.outro = stripped.to_string();
        }
        if !self.outro.is_empty() {
            return self;
        }
        let mut id_opt = Some(self.last_chunk);
        while let Some(id) = id_opt {
            if self.trim_chunk_end(id) {
                return self;
            }
            id_opt = self.chunk(id).previous;
        }
        self
    }

    /// Returns true if there's still content after trimming (so the
    /// outer loop should stop). Mirrors `Chunk.trimStart` for the
    /// non-aborted path.
    fn trim_chunk_start(&mut self, id: ChunkId) -> bool {
        // intro first
        let intro = std::mem::take(&mut self.chunks[id].intro);
        let trimmed_intro = intro.trim_start().to_string();
        self.chunks[id].intro = trimmed_intro.clone();
        if !trimmed_intro.is_empty() {
            return true;
        }
        // content
        let content = self.chunks[id].content.clone();
        let trimmed = content.trim_start().to_string();
        if !trimmed.is_empty() {
            if trimmed.len() != content.len() {
                self.chunks[id].content = trimmed;
            }
            return true;
        } else {
            self.chunks[id].content.clear();
            self.chunks[id].edited = true;
            // outro
            let outro = std::mem::take(&mut self.chunks[id].outro);
            let trimmed_outro = outro.trim_start().to_string();
            self.chunks[id].outro = trimmed_outro.clone();
            if !trimmed_outro.is_empty() {
                return true;
            }
        }
        false
    }

    fn trim_chunk_end(&mut self, id: ChunkId) -> bool {
        let outro = std::mem::take(&mut self.chunks[id].outro);
        let trimmed_outro = outro.trim_end().to_string();
        self.chunks[id].outro = trimmed_outro.clone();
        if !trimmed_outro.is_empty() {
            return true;
        }
        let content = self.chunks[id].content.clone();
        let trimmed = content.trim_end().to_string();
        if !trimmed.is_empty() {
            if trimmed.len() != content.len() {
                self.chunks[id].content = trimmed;
            }
            return true;
        } else {
            self.chunks[id].content.clear();
            self.chunks[id].edited = true;
            let intro = std::mem::take(&mut self.chunks[id].intro);
            let trimmed_intro = intro.trim_end().to_string();
            self.chunks[id].intro = trimmed_intro.clone();
            if !trimmed_intro.is_empty() {
                return true;
            }
        }
        false
    }

    /// Carve a `MagicString` from `start..end` of `self`, preserving any
    /// edits in that range. Mirrors `MagicString.snip(start, end)`.
    pub fn snip(&self, start: usize, end: usize) -> Self {
        let mut clone = self.clone_ms();
        // Remove [0, start) and (end, len].
        if start > 0 {
            clone.remove(0, start);
        }
        if end < self.original.len() {
            clone.remove(end, self.original.len());
        }
        clone
    }

    /// Split the chunk containing `index` so `index` becomes a boundary.
    /// Mirrors `MagicString._split` — no-op if `index` is already a
    /// boundary.
    fn _split(&mut self, index: usize) {
        if self.by_start.contains_key(&index) || self.by_end.contains_key(&index) {
            return;
        }
        let mut id_opt = Some(self.first_chunk);
        while let Some(id) = id_opt {
            let c = self.chunk(id);
            if c.start < index && c.end > index {
                self.split_chunk(id, index);
                return;
            }
            id_opt = c.next;
        }
    }

    fn split_chunk(&mut self, id: ChunkId, index: usize) -> ChunkId {
        let slice_index = index - self.chunk(id).start;
        let original = self.chunk(id).original.clone();
        let (before, after) = original.split_at(slice_index);
        let before = before.to_string();
        let after = after.to_string();

        let new_id = self.chunks.len();
        let end = self.chunk(id).end;
        let outro = std::mem::take(&mut self.chunk_mut(id).outro);
        let edited = self.chunk(id).edited;

        let mut new_chunk = Chunk::new(index, end, after);
        new_chunk.outro = outro;
        new_chunk.previous = Some(id);
        new_chunk.next = self.chunk(id).next;
        if edited {
            new_chunk.edit(String::new(), false);
        }
        self.chunks.push(new_chunk);

        // Patch the original chunk.
        self.chunk_mut(id).original = before.clone();
        self.chunk_mut(id).end = index;
        if edited {
            self.chunk_mut(id).content = String::new();
        } else {
            self.chunk_mut(id).content = before;
        }
        let old_next = self.chunk(id).next;
        if let Some(n) = old_next {
            self.chunk_mut(n).previous = Some(new_id);
        }
        self.chunk_mut(id).next = Some(new_id);

        self.by_end.insert(index, id);
        self.by_start.insert(index, new_id);
        self.by_end.insert(end, new_id);

        if self.last_chunk == id {
            self.last_chunk = new_id;
        }
        new_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_edits_round_trips() {
        let m = MagicString::new("hello world");
        assert_eq!(m.to_string(), "hello world");
    }

    #[test]
    fn append_outro() {
        let mut m = MagicString::new("hi");
        m.append(" there");
        assert_eq!(m.to_string(), "hi there");
    }

    #[test]
    fn prepend_intro() {
        let mut m = MagicString::new("world");
        m.prepend("hello ");
        assert_eq!(m.to_string(), "hello world");
    }

    #[test]
    fn overwrite_middle() {
        let mut m = MagicString::new("the quick brown fox");
        m.overwrite(4, 9, "slow");
        assert_eq!(m.to_string(), "the slow brown fox");
    }

    #[test]
    fn overwrite_at_start() {
        let mut m = MagicString::new("hello world");
        m.overwrite(0, 5, "HEY");
        assert_eq!(m.to_string(), "HEY world");
    }

    #[test]
    fn overwrite_at_end() {
        let mut m = MagicString::new("hello world");
        m.overwrite(6, 11, "MARS");
        assert_eq!(m.to_string(), "hello MARS");
    }

    #[test]
    fn remove_span() {
        let mut m = MagicString::new("hello world");
        m.remove(5, 11);
        assert_eq!(m.to_string(), "hello");
    }

    #[test]
    fn prepend_right_then_append_left() {
        let mut m = MagicString::new("hello world");
        m.prepend_right(5, " brave");
        assert_eq!(m.to_string(), "hello brave world");
    }

    #[test]
    fn append_left_inserts_with_chunk_left_of_index() {
        let mut m = MagicString::new("ab");
        m.append_left(1, "X");
        assert_eq!(m.to_string(), "aXb");
    }

    #[test]
    fn remove_after_prepend_left_keeps_left() {
        // prepend_left attaches to the chunk LEFT of index — so a remove
        // of the right span shouldn't drop it.
        let mut m = MagicString::new("ab");
        m.prepend_left(1, "X");
        m.remove(1, 2);
        assert_eq!(m.to_string(), "aX");
    }

    #[test]
    fn remove_after_prepend_right_drops_it() {
        // prepend_right attaches to the chunk RIGHT of index — so a
        // remove of the right span DOES drop it.
        let mut m = MagicString::new("ab");
        m.prepend_right(1, "X");
        m.remove(1, 2);
        assert_eq!(m.to_string(), "a");
    }

    #[test]
    fn clone_preserves_edits() {
        let mut m = MagicString::new("hello world");
        m.overwrite(0, 5, "HEY");
        let c = m.clone_ms();
        assert_eq!(c.to_string(), "HEY world");
    }

    #[test]
    fn clone_isolates_subsequent_edits() {
        let mut m = MagicString::new("hello world");
        m.overwrite(0, 5, "HEY");
        let mut c = m.clone_ms();
        c.overwrite(6, 11, "MARS");
        assert_eq!(m.to_string(), "HEY world");
        assert_eq!(c.to_string(), "HEY MARS");
    }

    #[test]
    fn slice_returns_original_when_no_edits() {
        let m = MagicString::new("hello world");
        assert_eq!(m.slice(0, 5), "hello");
    }

    #[test]
    fn slice_returns_edited_content() {
        let mut m = MagicString::new("hello world");
        m.overwrite(0, 5, "HEY");
        assert_eq!(m.slice(0, 5), "HEY");
    }

    #[test]
    fn overlapping_overwrite_then_append() {
        let mut m = MagicString::new("foo bar baz");
        m.overwrite(4, 7, "BAR");
        m.append_left(7, "!");
        assert_eq!(m.to_string(), "foo BAR! baz");
    }

    #[test]
    fn split_then_remove_one_chunk() {
        // remove the middle word
        let mut m = MagicString::new("foo bar baz");
        m.remove(3, 8); // " bar "
        assert_eq!(m.to_string(), "foobaz");
    }

    #[test]
    fn multiple_consecutive_inserts() {
        let mut m = MagicString::new("ab");
        m.append_left(1, "X");
        m.append_left(1, "Y");
        m.append_left(1, "Z");
        assert_eq!(m.to_string(), "aXYZb");
    }

    #[test]
    fn prepend_right_then_append_right_order() {
        let mut m = MagicString::new("ab");
        m.prepend_right(1, "[X]");
        m.append_right(1, "[Y]");
        // prepend_right prepends to chunk's intro, append_right appends:
        // intro = "[X]" + "[Y]"? Actually prepend prepends and append appends.
        // After prepend_right("[X]"): chunk.intro = "[X]"
        // After append_right("[Y]"): chunk.intro = "[X][Y]"
        assert_eq!(m.to_string(), "a[X][Y]b");
    }

    #[test]
    fn overwrite_preserves_clone_independence() {
        let mut m = MagicString::new("hello");
        let mut a = m.clone_ms();
        m.overwrite(0, 5, "HEY");
        a.overwrite(0, 5, "BYE");
        assert_eq!(m.to_string(), "HEY");
        assert_eq!(a.to_string(), "BYE");
    }

    #[test]
    fn is_empty_reports_correctly() {
        let m = MagicString::new("");
        assert!(m.is_empty());
        let m = MagicString::new("   \n  ");
        assert!(m.is_empty());
        let m = MagicString::new("x");
        assert!(!m.is_empty());
    }

    #[test]
    fn length_sums_to_string_len() {
        let mut m = MagicString::new("hello");
        assert_eq!(m.length(), 5);
        m.append("!");
        assert_eq!(m.length(), 6);
        m.prepend(">>");
        assert_eq!(m.length(), 8);
        assert_eq!(m.length(), m.to_string().len());
    }

    #[test]
    fn has_changed_after_edit() {
        let mut m = MagicString::new("foo");
        assert!(!m.has_changed());
        m.overwrite(0, 3, "bar");
        assert!(m.has_changed());
    }

    #[test]
    fn replace_first_occurrence() {
        let mut m = MagicString::new("foo bar foo");
        m.replace("foo", "BAZ");
        assert_eq!(m.to_string(), "BAZ bar foo");
    }

    #[test]
    fn replace_all_occurrences() {
        let mut m = MagicString::new("foo bar foo baz foo");
        m.replace_all("foo", "X");
        assert_eq!(m.to_string(), "X bar X baz X");
    }

    #[test]
    fn indent_prepends_after_newlines() {
        let mut m = MagicString::new("a\nb\nc");
        m.indent("  ");
        assert_eq!(m.to_string(), "  a\n  b\n  c");
    }

    #[test]
    fn trim_removes_whitespace() {
        let mut m = MagicString::new("   hello   ");
        m.trim();
        assert_eq!(m.to_string(), "hello");
    }

    #[test]
    fn trim_start_only() {
        let mut m = MagicString::new("  hello  ");
        m.trim_start();
        assert_eq!(m.to_string(), "hello  ");
    }

    #[test]
    fn trim_end_only() {
        let mut m = MagicString::new("  hello  ");
        m.trim_end();
        assert_eq!(m.to_string(), "  hello");
    }

    #[test]
    fn snip_extracts_range() {
        let m = MagicString::new("hello world");
        let s = m.snip(6, 11);
        assert_eq!(s.to_string(), "world");
    }

    #[test]
    fn generate_decoded_map_identity() {
        let m = MagicString::new("foo");
        let map = m.generate_decoded_map(GenerateMapOptions::default());
        assert_eq!(map.sources.len(), 1);
        assert!(!map.mappings.is_empty());
    }

    #[test]
    fn generate_map_emits_vlq_string() {
        let mut m = MagicString::new("foo");
        m.overwrite(0, 3, "BAR");
        let map = m.generate_map(GenerateMapOptions {
            file: Some("out.js".into()),
            source: Some("in.js".into()),
            include_content: true,
            hires: Hires::Off,
        });
        // Output is non-empty and printable ASCII (base64+;,).
        assert!(map.mappings.chars().all(|c| c.is_ascii() && !c.is_control()));
        assert_eq!(map.sources, vec!["in.js"]);
        assert_eq!(map.sources_content.as_deref(), Some(&["foo".to_string()][..]));
    }

    #[test]
    fn locate_returns_line_col() {
        let l = Locator::new("ab\ncd\nef");
        assert_eq!(l.locate(0), (0, 0));
        assert_eq!(l.locate(2), (0, 2)); // newline char
        assert_eq!(l.locate(3), (1, 0));
        assert_eq!(l.locate(6), (2, 0));
    }
}
