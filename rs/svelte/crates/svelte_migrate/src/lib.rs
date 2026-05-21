//! Migrate command — Svelte 4 → Svelte 5 source transformation.
//!
//! Mirrors `packages/svelte/src/compiler/migrate/`. Best-effort migration
//! that walks the AST and emits MagicString edits to convert legacy
//! patterns (`export let`, `$:`, `<slot>`, `on:` directives, etc.) into
//! the runes-mode equivalents (`$props()`, `$derived()`, `{@render}`,
//! event-attribute handlers).
//!
//! ## Status
//!
//! Scaffolding + the simplest transforms (e.g. stripping `accessors`
//! from `<svelte:options>`). The full port is multi-step — each
//! transform unlocks a cluster of fixtures.

#![forbid(unsafe_code)]

use svelte_magic_string::MagicString;

#[derive(Debug, Clone, Default)]
pub struct MigrateOptions {
    pub filename: Option<String>,
    pub use_ts: bool,
}

#[derive(Debug, Clone)]
pub struct MigrateResult {
    pub code: String,
}

/// Best-effort migration of Svelte 4 source towards Svelte 5 runes,
/// event attributes, and render tags. Returns the migrated source.
/// Currently a near-identity transform with a few surface-level edits;
/// the deeper rune / props / slot migration is staged port work.
pub fn migrate(source: &str, opts: MigrateOptions) -> MigrateResult {
    let _ = opts;
    let mut str = MagicString::new(source.to_string());

    // Surface-level edit: `<svelte:options ... accessors ...>` → strip
    // `accessors` and any single following whitespace. Mirrors upstream's
    // line 160: `str.replaceAll(/(<svelte:options\s.*?\s?)accessors\s?/g, $1)`.
    strip_accessors_in_svelte_options(source, &mut str);

    MigrateResult {
        code: str.to_string(),
    }
}

fn strip_accessors_in_svelte_options(source: &str, str: &mut MagicString) {
    let bytes = source.as_bytes();
    let needle = b"<svelte:options";
    let mut i = 0usize;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            let span = &source[i + needle.len()..j];
            if let Some(rel) = find_word(span, "accessors") {
                let abs_start = i + needle.len() + rel;
                let mut abs_end = abs_start + "accessors".len();
                if abs_end < bytes.len() && bytes[abs_end].is_ascii_whitespace() {
                    abs_end += 1;
                }
                let _ = str.remove(abs_start, abs_end);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
}

/// Find a standalone word in the search span — surrounded by whitespace,
/// `>` (close of attrs), or beginning-of-span boundaries.
fn find_word(span: &str, word: &str) -> Option<usize> {
    let bytes = span.as_bytes();
    let wbytes = word.as_bytes();
    let mut i = 0usize;
    while i + wbytes.len() <= bytes.len() {
        if &bytes[i..i + wbytes.len()] == wbytes {
            let before_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
            let after = i + wbytes.len();
            let after_ok = after == bytes.len()
                || bytes[after].is_ascii_whitespace()
                || bytes[after] == b'/'
                || bytes[after] == b'>';
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_accessors() {
        let src = "<svelte:options accessors immutable/>";
        let r = migrate(src, MigrateOptions::default());
        assert_eq!(r.code, "<svelte:options immutable/>");
    }

    #[test]
    fn identity_when_no_accessors() {
        let src = "<div>hi</div>";
        let r = migrate(src, MigrateOptions::default());
        assert_eq!(r.code, "<div>hi</div>");
    }
}
