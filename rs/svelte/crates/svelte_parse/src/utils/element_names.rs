//! Element-name predicates.
//!
//! Ported from `packages/svelte/src/utils.js`:
//! - `VOID_ELEMENT_NAMES` (lines 16-32)
//! - `is_void` (lines 38-40)
//! - `REGEX_VALID_TAG_NAME` (lines 493-494)

/// Mirrors `VOID_ELEMENT_NAMES`. Strings deliberately match upstream casing
/// — `is_void` does a case-insensitive comparison.
const VOID_ELEMENT_NAMES: &[&str] = &[
    "area", "base", "br", "col", "command", "embed", "hr", "img", "input", "keygen", "link",
    "meta", "param", "source", "track", "wbr",
];

/// `is_void(name)` from `packages/svelte/src/utils.js:38`. Matches upstream
/// behavior including the `!doctype` special case.
pub fn is_void(name: &str) -> bool {
    if VOID_ELEMENT_NAMES.iter().any(|v| v == &name) {
        return true;
    }
    name.eq_ignore_ascii_case("!doctype")
}

/// A simplified version of `REGEX_VALID_TAG_NAME`. Accepts:
/// - Standard elements: ASCII alpha start, then ASCII alphanumerics.
/// - Custom elements: ASCII alpha start, then alphanumerics with at least
///   one `-` somewhere (the upstream regex permits additional Unicode chars
///   after the hyphen — those are deferred until non-ASCII fixtures need them).
pub fn is_valid_tag_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    let mut has_hyphen = false;
    for &b in &bytes[1..] {
        match b {
            b'-' => has_hyphen = true,
            b'.' | b'_' if has_hyphen => {}
            c if c.is_ascii_alphanumeric() => {}
            _ => return false,
        }
    }
    let _ = has_hyphen;
    true
}

/// Returns true if seeing `next` as a child of `current` implicitly closes
/// `current`. Mirrors `closing_tag_omitted` in
/// `packages/svelte/src/html-tree-validation.js:63`.
///
/// Two flavours of relationship:
/// - "direct": child is auto-closed only when the next sibling is one of
///   these names. E.g. `<li>` is closed by another `<li>` sibling.
/// - "descendant": child is auto-closed when any descendant has one of these
///   names. E.g. `<p>` is closed by any block-level descendant.
pub fn closing_tag_omitted(current: &str, next: &str) -> bool {
    match current {
        "li" => matches!(next, "li"),
        "dt" | "dd" => matches!(next, "dt" | "dd"),
        "p" => matches!(
            next,
            "address"
                | "article"
                | "aside"
                | "blockquote"
                | "div"
                | "dl"
                | "fieldset"
                | "footer"
                | "form"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "header"
                | "hgroup"
                | "hr"
                | "main"
                | "menu"
                | "nav"
                | "ol"
                | "p"
                | "pre"
                | "section"
                | "table"
                | "ul"
        ),
        "rt" | "rp" => matches!(next, "rt" | "rp"),
        "optgroup" => matches!(next, "optgroup"),
        "option" => matches!(next, "option" | "optgroup"),
        "thead" => matches!(next, "tbody" | "tfoot"),
        "tbody" => matches!(next, "tbody" | "tfoot"),
        "tfoot" => matches!(next, "tbody"),
        "tr" => matches!(next, "tr" | "tbody"),
        "td" | "th" => matches!(next, "td" | "th" | "tr"),
        _ => false,
    }
}

/// Identifies a `svelte:foo` meta-tag name.
pub fn is_svelte_meta_name(name: &str) -> bool {
    name.starts_with("svelte:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn void_elements() {
        assert!(is_void("br"));
        assert!(is_void("input"));
        assert!(is_void("img"));
        assert!(is_void("!DOCTYPE"));
        assert!(is_void("!doctype"));
        assert!(!is_void("div"));
        assert!(!is_void("span"));
    }

    #[test]
    fn valid_tag_names() {
        assert!(is_valid_tag_name("div"));
        assert!(is_valid_tag_name("h1"));
        assert!(is_valid_tag_name("my-element"));
        assert!(!is_valid_tag_name(""));
        assert!(!is_valid_tag_name("1div"));
        assert!(!is_valid_tag_name("div>"));
    }

    #[test]
    fn svelte_meta_names() {
        assert!(is_svelte_meta_name("svelte:head"));
        assert!(is_svelte_meta_name("svelte:body"));
        assert!(!is_svelte_meta_name("div"));
    }
}
