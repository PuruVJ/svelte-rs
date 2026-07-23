//! Source-map generation for `MagicString`.
//!
//! Port of `generateDecodedMap` / `Mappings` from
//! `node_modules/magic-string@0.30.17/dist/magic-string.es.mjs:280-587`.

use std::collections::{HashMap, HashSet};

/// One mapping segment: `(generated_column, source_index, original_line, original_column)`.
/// The 5th element is `name_index` when names are tracked — modelled as a
/// separate `Vec<Option<u32>>` parallel to segments rather than a tagged
/// union, to keep the array shape stable.
pub type Segment = (u32, u32, u32, u32);

/// Decoded sourcemap suitable for VLQ-encoding into a v3 `mappings` string.
#[derive(Debug, Default)]
pub struct DecodedMap {
    pub file: Option<String>,
    pub sources: Vec<String>,
    pub sources_content: Option<Vec<String>>,
    pub names: Vec<String>,
    pub mappings: Vec<Vec<Segment>>,
}

/// Locate the (line, column) of a byte index inside `source`. Mirrors
/// `getLocator` from upstream — builds a `line → byte_offset` table and
/// binary-searches.
#[derive(Debug)]
pub struct Locator {
    line_offsets: Vec<usize>,
}

impl Locator {
    pub fn new(source: &str) -> Self {
        let mut line_offsets = Vec::with_capacity(source.len() / 40 + 1);
        let mut pos = 0usize;
        for line in source.split('\n') {
            line_offsets.push(pos);
            pos += line.len() + 1;
        }
        Self { line_offsets }
    }

    pub fn locate(&self, index: usize) -> (u32, u32) {
        let mut i = 0usize;
        let mut j = self.line_offsets.len();
        while i < j {
            let m = (i + j) / 2;
            if index < self.line_offsets[m] {
                j = m;
            } else {
                i = m + 1;
            }
        }
        let line = i.saturating_sub(1);
        let column = index - self.line_offsets[line];
        (line as u32, column as u32)
    }
}

/// Running accumulator for the mapping segments. Mirrors `class Mappings`.
#[derive(Debug)]
pub struct Mappings {
    hires: Hires,
    generated_line: usize,
    generated_column: u32,
    pub raw: Vec<Vec<Segment>>,
    /// Pending segment for "edit with empty content" — see upstream's
    /// `addEdit` where `this.pending` may carry forward to next iteration.
    pending: Option<Segment>,
}

#[derive(Debug, Clone, Copy)]
pub enum Hires {
    Off,
    On,
    Boundary,
}

impl Mappings {
    pub fn new(hires: Hires) -> Self {
        Self {
            hires,
            generated_line: 0,
            generated_column: 0,
            raw: vec![Vec::new()],
            pending: None,
        }
    }

    fn current_segments_mut(&mut self) -> &mut Vec<Segment> {
        if self.raw.len() <= self.generated_line {
            self.raw.resize_with(self.generated_line + 1, Vec::new);
        }
        &mut self.raw[self.generated_line]
    }

    /// Move the generated-position cursor forward by `str.len()`, breaking at
    /// newlines. Doesn't emit any segments — used between chunk content.
    pub fn advance(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let lines: Vec<&str> = s.split('\n').collect();
        if lines.len() > 1 {
            for _ in 0..(lines.len() - 1) {
                self.generated_line += 1;
                self.raw.push(Vec::new());
            }
            self.generated_column = 0;
        }
        self.generated_column += lines.last().unwrap().len() as u32;
    }

    /// Emit segments for an *edited* chunk — its content may be unrelated to
    /// the original, but it still maps back to the original `loc` start.
    pub fn add_edit(
        &mut self,
        source_index: u32,
        content: &str,
        loc: (u32, u32),
        name_index: Option<u32>,
    ) {
        if !content.is_empty() {
            // Walk newlines in content; emit one segment per generated line.
            let bytes = content.as_bytes();
            let mut content_line_end: i64 = bytes.iter().position(|&b| b == b'\n').map(|p| p as i64).unwrap_or(-1);
            let mut previous_line_end: i64 = -1;
            let cl_minus_one = content.len() as i64 - 1;

            while content_line_end >= 0 && cl_minus_one > content_line_end {
                let seg: Segment = (self.generated_column, source_index, loc.0, loc.1);
                self.current_segments_mut().push(seg);
                self.generated_line += 1;
                self.raw.push(Vec::new());
                self.generated_column = 0;

                previous_line_end = content_line_end;
                let next = bytes
                    .iter()
                    .skip((content_line_end + 1) as usize)
                    .position(|&b| b == b'\n')
                    .map(|p| (p as i64) + content_line_end + 1)
                    .unwrap_or(-1);
                content_line_end = next;
            }

            let seg: Segment = (self.generated_column, source_index, loc.0, loc.1);
            self.current_segments_mut().push(seg);

            self.advance(&content[(previous_line_end + 1) as usize..]);
            let _ = name_index;
        } else if let Some(pending) = self.pending.take() {
            self.current_segments_mut().push(pending);
            self.advance(content);
        }
        // upstream's `addEdit` zeros `pending` unconditionally at the end
        self.pending = None;
        let _ = loc;
    }

    /// Emit segments for an *unedited* chunk — content is sliced from the
    /// original source, so each character maps back to its own original
    /// position.
    pub fn add_unedited_chunk(
        &mut self,
        source_index: u32,
        chunk_start: usize,
        chunk_end: usize,
        original: &str,
        mut loc: (u32, u32),
        sourcemap_locations: &HashSet<usize>,
    ) {
        let bytes = original.as_bytes();
        let mut original_char_index = chunk_start;
        let mut first = true;
        let mut char_in_hires_boundary = false;

        while original_char_index < chunk_end {
            let b = bytes[original_char_index];
            if b == b'\n' {
                loc.0 += 1;
                loc.1 = 0;
                self.generated_line += 1;
                self.raw.push(Vec::new());
                self.generated_column = 0;
                first = true;
                char_in_hires_boundary = false;
            } else {
                let want = matches!(self.hires, Hires::On | Hires::Boundary)
                    || first
                    || sourcemap_locations.contains(&original_char_index);
                if want {
                    let seg: Segment = (self.generated_column, source_index, loc.0, loc.1);
                    match self.hires {
                        Hires::Boundary => {
                            let is_word = (b as char).is_alphanumeric() || b == b'_';
                            if is_word {
                                if !char_in_hires_boundary {
                                    self.current_segments_mut().push(seg);
                                    char_in_hires_boundary = true;
                                }
                            } else {
                                self.current_segments_mut().push(seg);
                                char_in_hires_boundary = false;
                            }
                        }
                        _ => {
                            self.current_segments_mut().push(seg);
                        }
                    }
                }

                loc.1 += 1;
                self.generated_column += 1;
                first = false;
            }
            original_char_index += 1;
        }
        self.pending = None;
    }
}

/// Encode decoded mappings into the VLQ-base64 form used by the v3 `mappings`
/// field. Mirrors `@jridgewell/sourcemap-codec`'s `encode()` output.
pub fn encode_mappings(mappings: &[Vec<Segment>]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(mappings.iter().map(|l| l.len() * 6 + 1).sum());
    let mut last_src_id: i64 = 0;
    let mut last_src_line: i64 = 0;
    let mut last_src_col: i64 = 0;

    for (i, line) in mappings.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        let mut last_gen_col: i64 = 0;
        for (j, seg) in line.iter().enumerate() {
            if j > 0 {
                out.push(b',');
            }
            let gen_col = seg.0 as i64;
            vlq_encode(gen_col - last_gen_col, &mut out);
            last_gen_col = gen_col;
            let src_id = seg.1 as i64;
            vlq_encode(src_id - last_src_id, &mut out);
            last_src_id = src_id;
            let src_line = seg.2 as i64;
            vlq_encode(src_line - last_src_line, &mut out);
            last_src_line = src_line;
            let src_col = seg.3 as i64;
            vlq_encode(src_col - last_src_col, &mut out);
            last_src_col = src_col;
        }
    }

    String::from_utf8(out).expect("VLQ output is ASCII")
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn vlq_encode(value: i64, out: &mut Vec<u8>) {
    let mut v: u64 = if value < 0 {
        (((-value) as u64) << 1) | 1
    } else {
        (value as u64) << 1
    };
    loop {
        let mut digit = (v & 0x1f) as u8;
        v >>= 5;
        if v != 0 {
            digit |= 0x20;
        }
        out.push(B64[digit as usize]);
        if v == 0 {
            break;
        }
    }
}

/// Suppress unused-warning bookkeeping during gradual rollout.
#[doc(hidden)]
pub fn _kludge_unused(_x: HashMap<usize, usize>) {}
