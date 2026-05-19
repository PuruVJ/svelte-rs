//! Phase 1: parse.
//!
//! Ported from `packages/svelte/src/compiler/phases/1-parse/`.
//!
//! Status: scaffolding + text + comment readers. Element, tag, script, and
//! style parsing land in 2c through 2g.

#![forbid(unsafe_code)]

pub mod hoist;
pub mod oxc_bridge;
pub mod oxc_to_typed;
pub mod parser;
pub mod state;
pub mod utils;

use svelte_ast::{
    Fragment, FragmentChild, JsComment, JsCommentKind, Position, Root, SourceLocation,
};
use svelte_diagnostics::CompileDiagnostic;

pub use parser::Parser;

/// Parse a `.svelte` source string. Mirrors the entry point at
/// `packages/svelte/src/compiler/phases/1-parse/index.js::parse`.
///
/// Status: handles BOM stripping + text + HTML comments at the top level.
/// Anything else falls through to text consumption — wrong, but harmless for
/// the cases this slice targets. Real element/tag/block/script parsing lands
/// in Phase 2c+.
pub fn parse(source: &str, loose: bool) -> Result<Root, CompileDiagnostic> {
    let source = Parser::strip_bom(source);
    let mut parser = Parser::new(source, loose);

    let fragment_start = parser.index as u32;
    let mut nodes: Vec<FragmentChild> = Vec::new();

    while parser.index < parser.template.len() {
        if parser.match_str("<") {
            // `<!--` is dispatched inside `read_element_or_comment`.
            nodes.push(state::element::read_element_or_comment(&mut parser)?);
            continue;
        }
        if parser.match_str("{") {
            nodes.push(state::tag::read_tag(&mut parser)?);
            continue;
        }
        let t = state::text::read_text(&mut parser);
        nodes.push(FragmentChild::Text(t));
    }

    let mut root = Root {
        css: None,
        js: vec![],
        start: 0,
        end: source.len() as u32,
        fragment: Fragment {
            // `fragment_start` is unused at the moment but will matter once
            // we track fragment boundaries inside blocks.
            nodes: {
                let _ = fragment_start;
                nodes
            },
        },
        options: None,
        comments: vec![],
        instance: None,
        module: None,
        parse_warnings: vec![],
    };

    // Hoist <script> and <style> children to Root.instance / module / css.
    // Mirrors the post-parse step at the bottom of upstream's parse() in
    // `phases/1-parse/index.js:153-168`.
    hoist::hoist_scripts_and_styles(&mut root, source, &parser.line_map, parser.ts)?;

    // Flush parser-collected comments (from `{expression}` mustaches and
    // element-attribute expressions) into `Root.comments`. Script-internal
    // comments are added by `hoist::build_script` separately.
    for c in std::mem::take(&mut parser.comments) {
        root.comments.push(raw_to_js_comment(&c, &parser.line_map));
    }
    root.parse_warnings.extend(std::mem::take(&mut parser.warnings));

    Ok(root)
}

fn raw_to_js_comment(
    c: &crate::oxc_bridge::RawComment,
    line_map: &crate::utils::locator::LineMap,
) -> JsComment {
    let (sl, sc) = line_map.locate(c.start as usize);
    let (el, ec) = line_map.locate(c.end as usize);
    let (start_char, end_char) = if c.with_character {
        (Some(c.start), Some(c.end))
    } else {
        (None, None)
    };
    JsComment {
        kind: if c.line {
            JsCommentKind::Line
        } else {
            JsCommentKind::Block
        },
        value: c.value.clone(),
        start: c.start,
        end: c.end,
        loc: SourceLocation {
            start: Position {
                line: sl,
                column: sc,
                character: start_char,
            },
            end: Position {
                line: el,
                column: ec,
                character: end_char,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use svelte_ast::FragmentChild;

    #[test]
    fn parses_empty_input() {
        let r = parse("", false).unwrap();
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 0);
        assert!(r.fragment.nodes.is_empty());
    }

    #[test]
    fn parses_plain_text() {
        let r = parse("hello", false).unwrap();
        assert_eq!(r.fragment.nodes.len(), 1);
        match &r.fragment.nodes[0] {
            FragmentChild::Text(t) => {
                assert_eq!(t.raw, "hello");
                assert_eq!(t.start, 0);
                assert_eq!(t.end, 5);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn parses_top_level_comment() {
        let r = parse("<!-- foo -->", false).unwrap();
        assert_eq!(r.fragment.nodes.len(), 1);
        match &r.fragment.nodes[0] {
            FragmentChild::Comment(c) => {
                assert_eq!(c.data, " foo ");
                assert_eq!(c.start, 0);
                assert_eq!(c.end, 12);
            }
            other => panic!("expected Comment, got {other:?}"),
        }
    }

    /// Root.end matches the full input length (after BOM strip), NOT the
    /// trim_end'd internal `parser.template.len()`. Upstream contract:
    /// `index.js:151` — `this.root.end = template.length` where `template`
    /// is the original constructor argument, not `this.template`.
    /// Callers that want the trimmed length must pre-trim (see
    /// `packages/svelte/tests/parser-modern/test.ts:14-17`).
    #[test]
    fn root_end_is_original_input_length() {
        let r = parse("hi\n  \n", false).unwrap();
        assert_eq!(r.end, 6);
    }

    #[test]
    fn strips_bom() {
        let r = parse("\u{feff}hi", false).unwrap();
        assert_eq!(r.end, 2);
    }
}
