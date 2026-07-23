//! HTML entity decoder.
//!
//! Ported subset of `packages/svelte/src/compiler/phases/1-parse/utils/html.js`
//! (`decode_character_references`). Covers:
//! - Named entities for the common cases (`&amp;`, `&lt;`, `&gt;`, `&quot;`,
//!   `&apos;`, `&nbsp;`, `&copy;`, `&reg;`, `&trade;`).
//! - Decimal numeric refs (`&#123;`).
//! - Hex numeric refs (`&#x7B;`).
//! - Attribute-value-specific rule: an entity *without* a trailing `;` is
//!   only decoded if the following character is not `=` and not an
//!   alphanumeric (matches html.spec.whatwg.org/multipage/parsing.html#named-character-reference-state).
//!
//! The full HTML named-entity table (~2300 entries in upstream `entities.js`)
//! is deferred; we add entries as fixtures demand them.

use std::collections::HashMap;
use std::sync::OnceLock;

fn named_entities() -> &'static HashMap<&'static str, char> {
    static MAP: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        // The 5 XML predefined entities + a handful of common HTML ones.
        m.insert("amp", '&');
        m.insert("AMP", '&');
        m.insert("lt", '<');
        m.insert("LT", '<');
        m.insert("gt", '>');
        m.insert("GT", '>');
        m.insert("quot", '"');
        m.insert("QUOT", '"');
        m.insert("apos", '\'');
        m.insert("nbsp", '\u{a0}');
        m.insert("copy", '\u{a9}');
        m.insert("COPY", '\u{a9}');
        m.insert("reg", '\u{ae}');
        m.insert("REG", '\u{ae}');
        m.insert("trade", '\u{2122}');
        m.insert("TRADE", '\u{2122}');
        m.insert("euro", '\u{20ac}');
        m.insert("pound", '\u{a3}');
        m.insert("yen", '\u{a5}');
        m.insert("cent", '\u{a2}');
        m.insert("sect", '\u{a7}');
        m.insert("para", '\u{b6}');
        m.insert("middot", '\u{b7}');
        m.insert("laquo", '\u{ab}');
        m.insert("raquo", '\u{bb}');
        m.insert("hellip", '\u{2026}');
        m.insert("ndash", '\u{2013}');
        m.insert("mdash", '\u{2014}');
        m.insert("lsquo", '\u{2018}');
        m.insert("rsquo", '\u{2019}');
        m.insert("ldquo", '\u{201c}');
        m.insert("rdquo", '\u{201d}');
        m
    })
}

/// Decode HTML character references in `input`. When `is_attribute_value` is
/// true, entities without a trailing `;` are kept verbatim if followed by
/// `=` or by another alphanumeric character.
pub fn decode_character_references(input: &str, is_attribute_value: bool) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'&' {
            // Push the byte as-is — it's a single UTF-8 byte unit in `out`.
            // For multi-byte chars, just push the char.
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        // Numeric reference: `&#NNN;` or `&#xHH;`.
        if i + 2 < bytes.len() && bytes[i + 1] == b'#' {
            let (consumed, ch) = parse_numeric_ref(&bytes[i + 2..]);
            if let Some(ch) = ch {
                out.push(ch);
                i += 2 + consumed; // past `&#` + digits + optional `;`
                continue;
            }
        }

        // Named reference: `&name;` or `&name` (with trailing-context rules).
        if let Some((name_len, ch, had_semi)) = parse_named_ref(&bytes[i + 1..]) {
            let after = i + 1 + name_len + if had_semi { 1 } else { 0 };
            // Attribute-value rule: keep verbatim if no `;` AND next char is
            // `=` or alphanumeric.
            let keep_verbatim = if !had_semi && is_attribute_value {
                bytes
                    .get(after)
                    .is_some_and(|b| *b == b'=' || b.is_ascii_alphanumeric())
            } else {
                false
            };
            if !keep_verbatim {
                out.push(ch);
                i = after;
                continue;
            }
        }

        out.push('&');
        i += 1;
    }
    out
}

/// Try to parse a decimal or hex numeric reference starting after `&#`.
/// Returns `(bytes_consumed_from_input, decoded_char_or_None)`.
fn parse_numeric_ref(bytes: &[u8]) -> (usize, Option<char>) {
    let (radix, offset) = match bytes.first() {
        Some(b'x') | Some(b'X') => (16u32, 1usize),
        _ => (10u32, 0usize),
    };

    let mut j = offset;
    let mut value: u32 = 0;
    while j < bytes.len() {
        let b = bytes[j];
        let digit = match b {
            b'0'..=b'9' => Some((b - b'0') as u32),
            b'a'..=b'f' if radix == 16 => Some(10 + (b - b'a') as u32),
            b'A'..=b'F' if radix == 16 => Some(10 + (b - b'A') as u32),
            _ => None,
        };
        match digit {
            Some(d) => {
                value = value.saturating_mul(radix).saturating_add(d);
                j += 1;
            }
            None => break,
        }
    }
    if j == offset {
        return (0, None); // no digits
    }

    // Optional trailing `;`.
    let semi = bytes.get(j) == Some(&b';');
    let consumed = j + if semi { 1 } else { 0 };

    match char::from_u32(value) {
        Some(c) => (consumed, Some(c)),
        None => (0, None),
    }
}

/// Try to parse a named reference starting after `&`. Returns
/// `(name_len, decoded_char, had_trailing_semicolon)`. Greedily takes the
/// longest matching entity name — but since our table only has ASCII names,
/// this is just "scan ASCII alpha chars".
fn parse_named_ref(bytes: &[u8]) -> Option<(usize, char, bool)> {
    // Collect ASCII alpha chars.
    let mut j = 0;
    while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j == 0 {
        return None;
    }
    let name = std::str::from_utf8(&bytes[..j]).ok()?;
    let had_semi = bytes.get(j) == Some(&b';');
    let map = named_entities();
    // Try the full name first, then shorter prefixes (greedy match).
    let mut len = j;
    while len > 0 {
        let candidate = &name[..len];
        if let Some(&ch) = map.get(candidate) {
            // Found — but if the match is shorter than the full alpha run AND
            // no semicolon, we can only match if the remaining run starts
            // some new character (we'll always extend; matches html spec).
            // For our purposes, prefer the full-length match.
            return Some((len, ch, had_semi && len == j));
        }
        len -= 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_amp() {
        assert_eq!(decode_character_references("a &amp; b", false), "a & b");
    }

    #[test]
    fn decodes_nbsp() {
        assert_eq!(decode_character_references("&nbsp;", false), "\u{a0}");
    }

    #[test]
    fn decodes_numeric() {
        assert_eq!(decode_character_references("&#65;", false), "A");
        assert_eq!(decode_character_references("&#x41;", false), "A");
        assert_eq!(decode_character_references("&#X41;", false), "A");
    }

    #[test]
    fn keeps_unknown_entity() {
        assert_eq!(decode_character_references("&xyz;", false), "&xyz;");
    }

    #[test]
    fn attr_keeps_unterminated_before_equal() {
        // `&quot=` — followed by `=`, keep verbatim in attr.
        assert_eq!(
            decode_character_references("&quot=", true),
            "&quot="
        );
    }

    #[test]
    fn attr_keeps_unterminated_before_alnum() {
        // `&quot1` — followed by digit, keep verbatim.
        assert_eq!(
            decode_character_references("&quot1", true),
            "&quot1"
        );
    }

    #[test]
    fn attr_decodes_terminated() {
        assert_eq!(decode_character_references("&quot;", true), "\"");
    }

    #[test]
    fn attr_decodes_followed_by_space() {
        // `&quot ` — no `;`, but followed by space (not alnum, not `=`).
        // Per upstream rule, this still decodes.
        assert_eq!(decode_character_references("&quot ", true), "\" ");
    }
}
