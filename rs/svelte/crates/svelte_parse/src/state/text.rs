//! Text reader.
//!
//! Ported from `packages/svelte/src/compiler/phases/1-parse/state/text.js`.
//! Reads characters until the next `<` or `{` (or EOF), then emits a `Text`
//! node carrying both the raw source slice and the entity-decoded form.

use svelte_ast::{Text, };

use crate::parser::Parser;
use crate::utils::entities::decode_character_references;

pub fn read_text(parser: &mut Parser<'_>) -> Text {
    let start = parser.index as u32;

    let bytes = parser.template.as_bytes();
    while parser.index < parser.template.len() {
        let b = bytes[parser.index];
        if b == b'<' || b == b'{' {
            break;
        }
        // Advance by one full char to stay UTF-8-safe.
        let ch = parser.template[parser.index..].chars().next().unwrap();
        parser.index += ch.len_utf8();
    }

    let raw = &parser.template[start as usize..parser.index];
    Text {
        start,
        end: parser.index as u32,
        raw: raw.into(),
        data: decode_character_references(raw, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_ascii_text() {
        let mut p = Parser::new("hello world", false);
        let t = read_text(&mut p);
        assert_eq!(t.raw, "hello world");
        assert_eq!(t.data, "hello world");
        assert_eq!(t.start, 0);
        assert_eq!(t.end, 11);
        assert_eq!(p.index, 11);
    }

    #[test]
    fn stops_at_open_bracket() {
        let mut p = Parser::new("hello{x}", false);
        let t = read_text(&mut p);
        assert_eq!(t.raw, "hello");
        assert_eq!(t.end, 5);
        assert_eq!(p.index, 5);
    }

    #[test]
    fn stops_at_less_than() {
        let mut p = Parser::new("hi<div>", false);
        let t = read_text(&mut p);
        assert_eq!(t.raw, "hi");
        assert_eq!(t.end, 2);
    }

    #[test]
    fn handles_multibyte_chars() {
        let mut p = Parser::new("héllo<", false);
        let t = read_text(&mut p);
        assert_eq!(t.raw, "héllo");
        // "héllo" = h(1) + é(2) + l(1) + l(1) + o(1) = 6 bytes
        assert_eq!(t.end, 6);
    }
}
