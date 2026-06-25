//! Whitespace classification.
//!
//! Ported from `packages/svelte/src/compiler/phases/1-parse/index.js:14-31`
//! (`is_whitespace`). Same character set, same fast-path.

/// Matches the upstream `is_whitespace(cc)`:
/// - `\t`, `\n`, `\v`, `\f`, `\r`, space (codepoints 9-13, 32)
/// - Unicode space separators above 0x9f (NBSP, ZWNBSP, etc.)
pub fn is_whitespace(c: char) -> bool {
    let cc = c as u32;
    if cc == 32 || (cc <= 13 && cc >= 9) {
        return true;
    }
    if cc < 160 {
        return false;
    }
    matches!(
        cc,
        160 | 5760
            | 8192..=8202
            | 8232
            | 8233
            | 8239
            | 8287
            | 12288
            | 65279
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_whitespace() {
        for c in " \t\n\r\x0b\x0c".chars() {
            assert!(is_whitespace(c), "expected whitespace for {c:?}");
        }
        assert!(!is_whitespace('a'));
        assert!(!is_whitespace('0'));
    }

    #[test]
    fn unicode_whitespace() {
        assert!(is_whitespace('\u{00a0}')); // NBSP
        assert!(is_whitespace('\u{2028}')); // LINE SEPARATOR
        assert!(is_whitespace('\u{feff}')); // ZWNBSP / BOM
        assert!(!is_whitespace('\u{00a1}'));
    }
}
