//! HTML entity decoding stub.
//!
//! Ported skeleton of `packages/svelte/src/compiler/phases/1-parse/utils/html.js`
//! (`decode_character_references`). Full entity tables land in a follow-up.
//! For now: pass-through. This is good enough for fixtures whose text has no
//! `&entity;` references.
//!
//! TODO: port the full named-entity table from `utils/entities.js`.

pub fn decode_character_references(input: &str, _attr: bool) -> String {
    // TODO(svelte_parse): full HTML entity decoding.
    input.to_string()
}
