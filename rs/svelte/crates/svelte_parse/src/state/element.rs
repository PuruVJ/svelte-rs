//! Element parser.
//!
//! Ported (partially) from `packages/svelte/src/compiler/phases/1-parse/state/element.js`.
//! Current coverage:
//! - Open/close tags: `<div>...</div>`
//! - Self-closing: `<br/>`
//! - Void elements: `<br>`, `<input>`, etc. — no closing tag required.
//! - Nested elements (recursive fragment parsing).
//! - Bare attributes: `<input disabled>`
//! - Double/single-quoted string attributes: `<div class="x">`, `<div class='x'>`
//!
//! Not yet covered (deferred to follow-ups):
//! - Component invocation (uppercase tag names → `Component`).
//! - `svelte:*` meta tags → `SvelteBody/Boundary/Component/...`.
//! - Expression attributes: `name={expr}`, `{...spread}`, directives
//!   (`bind:`, `on:`, `use:`, `transition:`, `in:`, `out:`, `animate:`,
//!   `class:`, `let:`, `style:`).
//! - Unquoted attribute values.
//! - HTML entity decoding inside attribute values (currently pass-through).
//! - Auto-closing logic (`<p><p>` closes the first one).

use svelte_ast::{
    Attribute, AttributeKind, AttributeValue, AttributeValuePart, ElementAttribute, Fragment,
    FragmentChild, FragmentKind, RegularElement, RegularElementKind, SourceLocation, Text,
    TextKind,
};
use svelte_diagnostics::{errors, CompileDiagnostic};

use crate::parser::Parser;
use crate::utils::element_names::{is_svelte_meta_name, is_valid_tag_name, is_void};

/// Reads a `<...>` construct starting at the current cursor.
///
/// Caller is expected to have peeked `<` but NOT consumed it.
///
/// Returns `FragmentChild::Comment` for `<!-- -->`, otherwise a
/// `FragmentChild::RegularElement` (component / special-element variants are
/// deferred — they fall back to `RegularElement` shape for now).
pub fn read_element_or_comment(
    parser: &mut Parser<'_>,
) -> Result<FragmentChild, CompileDiagnostic> {
    debug_assert!(parser.match_str("<"));
    let start = parser.index;

    // Comment short-circuit: `<!--` is handled by `state/comment.rs`.
    if parser.template[parser.index..].starts_with("<!--") {
        let c = super::comment::read_comment(parser)?;
        return Ok(FragmentChild::Comment(c));
    }

    // Consume `<`.
    parser.index += 1;

    if parser.match_str("/") {
        return Err(errors::expected_token(
            Some((start as u32, start as u32)),
            "element name",
        ));
    }

    let name = read_tag_name(parser)?;

    if is_svelte_meta_name(&name) {
        // TODO(2c follow-up): SvelteBody / SvelteHead / etc.
        // For now, fall through and treat as a RegularElement — the harness
        // diff will surface the divergence on those fixtures.
    }

    let name_loc = SourceLocation {
        start: parser.line_map.position(start + 1), // after `<`
        end: parser.line_map.position(start + 1 + name.len()),
    };

    let attributes = read_attributes(parser)?;

    parser.allow_whitespace();

    // Self-closing `/>`.
    let self_closing = parser.eat("/>");

    if !self_closing {
        // Otherwise `>` is required.
        if !parser.eat(">") {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                ">",
            ));
        }
    }

    // Void elements never have a body.
    if self_closing || is_void(&name) {
        return Ok(FragmentChild::RegularElement(RegularElement {
            kind: RegularElementKind::RegularElement,
            start: start as u32,
            end: parser.index as u32,
            name: name.into_owned(),
            name_loc,
            attributes,
            fragment: Fragment::empty(),
        }));
    }

    // Parse children until matching `</name>`.
    let fragment = parse_fragment_until_close_tag(parser, &name)?;

    // Consume `</name>`.
    if !parser.eat("</") {
        return Err(errors::element_unclosed(
            Some((start as u32, (start + 1) as u32)),
            &name,
        ));
    }
    let close_start = parser.index;
    let close_name = read_tag_name(parser)?;
    if close_name != name {
        return Err(errors::element_invalid_closing_tag(
            Some((close_start as u32, parser.index as u32)),
            &close_name,
        ));
    }
    parser.allow_whitespace();
    if !parser.eat(">") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            ">",
        ));
    }

    Ok(FragmentChild::RegularElement(RegularElement {
        kind: RegularElementKind::RegularElement,
        start: start as u32,
        end: parser.index as u32,
        name: name.into_owned(),
        name_loc,
        attributes,
        fragment,
    }))
}

/// Read a tag name. Returns the slice consumed (borrowed from the source).
fn read_tag_name<'src>(parser: &mut Parser<'src>) -> Result<std::borrow::Cow<'src, str>, CompileDiagnostic> {
    let start = parser.index;
    // Tag-name characters: alphanumerics, `-`, `.`, `_`, `:` (for svelte:foo).
    let name = parser.read_while(|b| {
        b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b':' || b == b'!'
    });
    if !is_valid_tag_name(name) && !is_svelte_meta_name(name) && !name.eq_ignore_ascii_case("!doctype") {
        return Err(errors::expected_token(
            Some((start as u32, parser.index as u32)),
            "valid tag name",
        ));
    }
    Ok(std::borrow::Cow::Borrowed(name))
}

/// Read all attributes up to (but not including) `>` or `/>`.
fn read_attributes(parser: &mut Parser<'_>) -> Result<Vec<ElementAttribute>, CompileDiagnostic> {
    let mut out = Vec::new();
    loop {
        parser.allow_whitespace();
        if parser.match_str(">") || parser.match_str("/>") || parser.peek().is_none() {
            break;
        }
        // TODO(2d): proper parsing for `{...spread}` and `{name}` shorthand
        // and directives. For now, when we encounter a `{` inside an opening
        // tag we skip to the matching `}` so the rest of the tag continues
        // to parse — keeps the harness output in "diff" territory rather
        // than spuriously erroring out.
        if parser.peek() == Some(b'{') {
            skip_braced(parser)?;
            continue;
        }
        out.push(read_attribute(parser)?);
    }
    Ok(out)
}

/// Skip a `{...}` block, balancing nested braces. Used as a placeholder until
/// real mustache parsing lands in Phase 2d.
fn skip_braced(parser: &mut Parser<'_>) -> Result<(), CompileDiagnostic> {
    let start = parser.index;
    debug_assert_eq!(parser.peek(), Some(b'{'));
    parser.index += 1;
    let mut depth = 1_usize;
    while parser.index < parser.template.len() && depth > 0 {
        let b = parser.template.as_bytes()[parser.index];
        match b {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        parser.index += 1;
    }
    if depth != 0 {
        return Err(errors::expected_token(
            Some((start as u32, parser.index as u32)),
            "}",
        ));
    }
    Ok(())
}

fn read_attribute(parser: &mut Parser<'_>) -> Result<ElementAttribute, CompileDiagnostic> {
    let start = parser.index;

    // Attribute name: any byte that isn't whitespace, `=`, `/`, `>`, `"`, `'`.
    let name_start = parser.index;
    let name = parser.read_while(|b| {
        !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'=' | b'/' | b'>' | b'"' | b'\'')
    });
    if name.is_empty() {
        return Err(errors::expected_token(
            Some((start as u32, parser.index as u32)),
            "attribute name",
        ));
    }
    let name_end = parser.index;
    let name = name.to_string();

    let name_loc = SourceLocation {
        start: parser.line_map.position(name_start),
        end: parser.line_map.position(name_end),
    };

    let value = if parser.eat("=") {
        parser.allow_whitespace();
        read_attribute_value(parser)?
    } else {
        AttributeValue::Empty(true)
    };

    let end = parser.index;
    Ok(ElementAttribute::Attribute(Attribute {
        kind: AttributeKind::Attribute,
        start: start as u32,
        end: end as u32,
        name,
        name_loc: Some(name_loc),
        value,
    }))
}

fn read_attribute_value(parser: &mut Parser<'_>) -> Result<AttributeValue, CompileDiagnostic> {
    // Mustache-only value: `name={expr}`. Real parsing comes in Phase 2d; for
    // now consume the balanced `{...}` and represent it as a placeholder text
    // part so the rest of the tag parses cleanly.
    if parser.peek() == Some(b'{') {
        let start = parser.index;
        skip_braced(parser)?;
        let end = parser.index;
        let raw = parser.template[start..end].to_string();
        return Ok(AttributeValue::Many(vec![AttributeValuePart::Text(Text {
            kind: TextKind::Text,
            start: start as u32,
            end: end as u32,
            raw: raw.clone(),
            data: raw,
        })]));
    }

    // Quoted form: "..." or '...'. Match the leading quote, accumulate until
    // the matching trailing quote.
    let opener = parser.peek();
    let quote = match opener {
        Some(b'"') => Some(b'"'),
        Some(b'\'') => Some(b'\''),
        _ => None,
    };

    let Some(quote) = quote else {
        // Unquoted attribute value — read until whitespace/`>`.
        // TODO(2c follow-up): support unquoted values + expression chunks.
        let start = parser.index;
        let raw = parser.read_while(|b| {
            !matches!(
                b,
                b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/' | b'"' | b'\''
            )
        });
        let end = parser.index;
        if raw.is_empty() {
            return Err(errors::expected_token(
                Some((start as u32, end as u32)),
                "attribute value",
            ));
        }
        return Ok(AttributeValue::Many(vec![AttributeValuePart::Text(Text {
            kind: TextKind::Text,
            start: start as u32,
            end: end as u32,
            raw: raw.to_string(),
            data: raw.to_string(),
        })]));
    };

    // Consume the opening quote.
    parser.index += 1;
    let content_start = parser.index;

    let mut parts: Vec<AttributeValuePart> = Vec::new();
    let mut text_start = content_start;

    loop {
        let Some(b) = parser.peek() else {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                "matching quote",
            ));
        };
        if b == quote {
            break;
        }
        // TODO(2d): handle `{` to start a mustache expression chunk.
        // For now, treat everything up to the matching quote as text.
        parser.index += 1;
    }

    let text_end = parser.index;
    if text_end > text_start {
        let raw = &parser.template[text_start..text_end];
        parts.push(AttributeValuePart::Text(Text {
            kind: TextKind::Text,
            start: text_start as u32,
            end: text_end as u32,
            raw: raw.to_string(),
            data: raw.to_string(), // TODO(2c): decode entities
        }));
    }
    let _ = text_start;

    // Consume closing quote.
    parser.index += 1;

    Ok(AttributeValue::Many(parts))
}

/// Recursively parse a fragment until the parser cursor sits at `</tag_name>`.
fn parse_fragment_until_close_tag(
    parser: &mut Parser<'_>,
    tag_name: &str,
) -> Result<Fragment, CompileDiagnostic> {
    let mut nodes: Vec<FragmentChild> = Vec::new();
    loop {
        if parser.index >= parser.template.len() {
            // EOF before close tag.
            return Err(errors::element_unclosed(
                Some((parser.index as u32, parser.index as u32)),
                tag_name,
            ));
        }

        // Close-tag for *this* element terminates the fragment.
        if parser.match_str("</")
            && parser.template[parser.index + 2..]
                .strip_prefix(tag_name)
                .is_some_and(|rest| {
                    let bytes = rest.as_bytes();
                    bytes.is_empty()
                        || matches!(
                            bytes[0],
                            b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/'
                        )
                })
        {
            break;
        }

        if parser.match_str("<!--") {
            let c = super::comment::read_comment(parser)?;
            nodes.push(FragmentChild::Comment(c));
            continue;
        }
        if parser.match_str("<") {
            let child = read_element_or_comment(parser)?;
            nodes.push(child);
            continue;
        }
        if parser.match_str("{") {
            // TODO(2d): mustache parsing. For now skip one byte to avoid loop.
            let ch = parser.template[parser.index..].chars().next().unwrap();
            parser.index += ch.len_utf8();
            continue;
        }
        let t = super::text::read_text(parser);
        nodes.push(FragmentChild::Text(t));
    }
    Ok(Fragment {
        kind: FragmentKind::Fragment,
        nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn first_node(input: &str) -> FragmentChild {
        let r = parse(input, false).unwrap();
        r.fragment.nodes.into_iter().next().unwrap()
    }

    #[test]
    fn empty_div() {
        let n = first_node("<div></div>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "div");
                assert_eq!(el.start, 0);
                assert_eq!(el.end, 11);
                assert!(el.attributes.is_empty());
                assert!(el.fragment.nodes.is_empty());
                assert_eq!(el.name_loc.start.line, 1);
                assert_eq!(el.name_loc.start.column, 1);
                assert_eq!(el.name_loc.start.character, Some(1));
                assert_eq!(el.name_loc.end.character, Some(4));
            }
            other => panic!("expected RegularElement, got {other:?}"),
        }
    }

    #[test]
    fn self_closing_br() {
        let n = first_node("<br/>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "br");
                assert_eq!(el.end, 5);
                assert!(el.fragment.nodes.is_empty());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn void_element_without_slash() {
        let n = first_node("<br>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "br");
                assert_eq!(el.end, 4);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn element_with_text_child() {
        let n = first_node("<p>hello</p>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "p");
                assert_eq!(el.fragment.nodes.len(), 1);
                match &el.fragment.nodes[0] {
                    FragmentChild::Text(t) => assert_eq!(t.raw, "hello"),
                    o => panic!("expected Text, got {o:?}"),
                }
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn nested_elements() {
        let n = first_node("<div><span>x</span></div>");
        let outer = match n {
            FragmentChild::RegularElement(el) => el,
            other => panic!("got {other:?}"),
        };
        assert_eq!(outer.name, "div");
        assert_eq!(outer.fragment.nodes.len(), 1);
        let inner = match &outer.fragment.nodes[0] {
            FragmentChild::RegularElement(el) => el,
            other => panic!("expected inner RegularElement, got {other:?}"),
        };
        assert_eq!(inner.name, "span");
        assert_eq!(inner.fragment.nodes.len(), 1);
    }

    #[test]
    fn bare_attribute() {
        let n = first_node("<input disabled>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.attributes.len(), 1);
                match &el.attributes[0] {
                    ElementAttribute::Attribute(a) => {
                        assert_eq!(a.name, "disabled");
                        assert!(matches!(a.value, AttributeValue::Empty(true)));
                    }
                    other => panic!("expected Attribute, got {other:?}"),
                }
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn quoted_attribute_value() {
        let n = first_node(r#"<div class="foo bar"></div>"#);
        let el = match n {
            FragmentChild::RegularElement(e) => e,
            other => panic!("got {other:?}"),
        };
        assert_eq!(el.attributes.len(), 1);
        match &el.attributes[0] {
            ElementAttribute::Attribute(a) => {
                assert_eq!(a.name, "class");
                match &a.value {
                    AttributeValue::Many(parts) => {
                        assert_eq!(parts.len(), 1);
                        match &parts[0] {
                            AttributeValuePart::Text(t) => {
                                assert_eq!(t.raw, "foo bar");
                            }
                            o => panic!("expected Text, got {o:?}"),
                        }
                    }
                    o => panic!("expected Many, got {o:?}"),
                }
            }
            o => panic!("expected Attribute, got {o:?}"),
        }
    }

    #[test]
    fn multiple_attributes() {
        let n = first_node(r#"<input type="text" name="email" required>"#);
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.attributes.len(), 3);
            }
            other => panic!("got {other:?}"),
        }
    }
}
