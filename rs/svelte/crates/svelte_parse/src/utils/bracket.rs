//! Bracket matcher.
//!
//! Ported from `packages/svelte/src/compiler/phases/1-parse/utils/bracket.js`.
//! Used by `read_pattern` (for `{#each ... as <pattern>}`) to find where a
//! `{...}` / `[...]` / `(...)` destructuring pattern ends.
//!
//! Quote-, template-literal-, and nested-bracket-aware. Regex literals are
//! NOT handled — they don't appear inside destructuring patterns. Comments
//! are NOT handled either; patterns don't contain them.

/// Find the index of the matching closing bracket for the opener at
/// `template[open_index]`. The opener must be `{`, `[`, or `(`.
///
/// Returns the byte index of the matching close, or `None` if the source ends
/// before the bracket closes.
pub fn find_matching_bracket(template: &str, open_index: usize) -> Option<usize> {
    let bytes = template.as_bytes();
    if open_index >= bytes.len() {
        return None;
    }
    let opener = bytes[open_index];
    let closer = match opener {
        b'{' => b'}',
        b'[' => b']',
        b'(' => b')',
        _ => return None,
    };

    let mut i = open_index + 1;
    let mut depth: i32 = 1;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                i = skip_string(bytes, i + 1, b);
            }
            b'`' => {
                i = skip_template(bytes, i + 1);
            }
            c if c == opener => {
                depth += 1;
                i += 1;
            }
            c if c == closer => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            // Track all bracket kinds — `{[}]` is invalid JS, but we still
            // need to make sure depths are tracked per pair.
            b'{' | b'[' | b'(' => {
                let nested_end = find_matching_bracket(template, i)?;
                i = nested_end + 1;
            }
            // Stray `}`, `]`, `)` that don't match our opener — bail.
            b'}' | b']' | b')' => return None,
            _ => i += 1,
        }
    }
    None
}

fn skip_string(bytes: &[u8], mut i: usize, quote: u8) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Skip past a template literal body. `i` is positioned after the opening
/// backtick; returns the index *after* the closing backtick.
///
/// Recurses into `${...}` interpolations via `find_matching_bracket` so that
/// `` `Jane ${"Doe"}` `` is handled correctly.
fn skip_template(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                // i points at `$`; the `{` is at i+1.
                let template_str = std::str::from_utf8(bytes).unwrap_or("");
                match find_matching_bracket(template_str, i + 1) {
                    Some(end) => i = end + 1,
                    None => return bytes.len(),
                }
            }
            b'`' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Find the closing `>` for an opening `<` at `open_index`. Used to parse
/// TypeScript generic parameters in `{#snippet foo<T>(...)}` etc.
///
/// Tracks nested `< >` (e.g. `Set<"<">>`), `()`, `[]`, `{}`, plus strings.
pub fn find_matching_pointy(template: &str, open_index: usize) -> Option<usize> {
    let bytes = template.as_bytes();
    if open_index >= bytes.len() || bytes[open_index] != b'<' {
        return None;
    }
    let mut i = open_index + 1;
    let mut depth: i32 = 1;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => i = skip_string(bytes, i + 1, b),
            b'`' => i = skip_template(bytes, i + 1),
            b'<' => {
                depth += 1;
                i += 1;
            }
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            b'(' | b'[' | b'{' => {
                let template_str = std::str::from_utf8(bytes).ok()?;
                let nested_end = find_matching_bracket(template_str, i)?;
                i = nested_end + 1;
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_object_pattern() {
        let s = "{ name, cool = true } trailing";
        assert_eq!(find_matching_bracket(s, 0), Some(20));
    }

    #[test]
    fn matches_array_pattern() {
        let s = "[a, b = 1] trailing";
        assert_eq!(find_matching_bracket(s, 0), Some(9));
    }

    #[test]
    fn handles_nested_brackets() {
        let s = "{ a: { b: 1 } } trailing";
        assert_eq!(find_matching_bracket(s, 0), Some(14));
    }

    #[test]
    fn handles_strings_with_braces() {
        let s = "{ a: '}' } trailing";
        assert_eq!(find_matching_bracket(s, 0), Some(9));
    }

    #[test]
    fn handles_template_literal_with_interpolation() {
        let s = "{ a: `x ${1+1}` } trailing";
        assert_eq!(find_matching_bracket(s, 0), Some(16));
    }
}
