//! Mustache tag parser — `{expression}`, `{@html ...}`, `{@attach ...}`,
//! `{@const ...}`, `{@debug ...}`, `{@render ...}`.
//!
//! Ported (partially) from
//! `packages/svelte/src/compiler/phases/1-parse/state/tag.js`.
//!
//! Current coverage:
//! - `{expression}` → `ExpressionTag` (uses OXC to parse the inner expression).
//! - `{@html expr}` → `HtmlTag`.
//! - `{@attach expr}` → `AttachTag`.
//!
//! Deferred:
//! - `{@const VariableDeclaration}` — requires statement-level parsing.
//! - `{@debug ids}` — Identifier list.
//! - `{@render expr(...)}` — needs `CallExpression` shape validation.
//! - `{#if}`, `{#each}`, `{#await}`, `{#key}`, `{#snippet}` — Phase 2e.
//! - `{:else}`, `{:then}`, `{:catch}` continuations — Phase 2e.
//! - `{/if}`, `{/each}` close tags — Phase 2e.
//! - `{:...}` and `{/...}` are flagged as errors here so they don't get silently
//!   misparsed as expressions; Phase 2e will handle them properly.

use svelte_ast::{
    AttachTag, AttachTagKind, ExpressionTag, ExpressionTagKind, FragmentChild, HtmlTag, HtmlTagKind,
    RenderTag, RenderTagKind,
};
use svelte_diagnostics::{errors, CompileDiagnostic};

use crate::parser::Parser;
use crate::state::blocks;

/// Reads a `{...}` mustache. Assumes the cursor sits on `{`.
pub fn read_tag(parser: &mut Parser<'_>) -> Result<FragmentChild, CompileDiagnostic> {
    debug_assert!(parser.match_str("{"));
    let start = parser.index;
    parser.index += 1; // consume `{`

    parser.skip_whitespace_and_js_comments();

    // `{#if ...}`, `{#each ...}`, `{#key ...}`, etc.
    if parser.peek() == Some(b'#') {
        return blocks::read_block_open(parser, start);
    }

    // `{:else}`, `{:then}`, `{:catch}` — block continuations, must appear
    // inside a block and are handled by the block parser's recursion.
    // Reaching them here means a stray continuation outside any block.
    if parser.peek() == Some(b':') {
        return Err(errors::expected_token(
            Some((start as u32, parser.index as u32)),
            "block continuation inside a matching block",
        ));
    }

    // `{/if}`, `{/each}`, etc. — same as `:` — unexpected here. Note that
    // `{/* comment */}` is NOT a close tag (it's a JS block comment inside
    // an expression); we already skipped past it via
    // `skip_whitespace_and_js_comments` above.
    if parser.peek() == Some(b'/') {
        return Err(errors::expected_token(
            Some((start as u32, parser.index as u32)),
            "matching open block before close tag",
        ));
    }

    // At-tags: `{@html}`, `{@const}`, `{@debug}`, `{@render}`, `{@attach}`.
    if parser.peek() == Some(b'@') {
        return read_at_tag(parser, start);
    }

    // Plain `{expression}`.
    let (expr_json, expr_end) = parser.parse_expression_at(parser.index)?;
    parser.index = expr_end;
    parser.skip_whitespace_and_js_comments();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }
    Ok(FragmentChild::ExpressionTag(ExpressionTag {
        kind: ExpressionTagKind::ExpressionTag,
        start: start as u32,
        end: parser.index as u32,
        expression: expr_json,
    }))
}

fn read_at_tag(
    parser: &mut Parser<'_>,
    start: usize,
) -> Result<FragmentChild, CompileDiagnostic> {
    debug_assert_eq!(parser.peek(), Some(b'@'));
    parser.index += 1; // consume `@`
    let name_start = parser.index;
    let name = parser.read_while(|b| b.is_ascii_alphanumeric() || b == b'_');
    let name = name.to_string();
    let name_end = parser.index;

    match name.as_str() {
        "html" | "attach" => {
            parser.allow_whitespace();
            let (expr_json, expr_end) = parser.parse_expression_at(parser.index)?;
            parser.index = expr_end;
            parser.allow_whitespace();
            if !parser.eat("}") {
                return Err(errors::expected_token(
                    Some((parser.index as u32, parser.index as u32)),
                    "}",
                ));
            }
            let end = parser.index as u32;
            if name == "html" {
                Ok(FragmentChild::HtmlTag(HtmlTag {
                    kind: HtmlTagKind::HtmlTag,
                    start: start as u32,
                    end,
                    expression: expr_json,
                }))
            } else {
                Ok(FragmentChild::AttachTag(AttachTag {
                    kind: AttachTagKind::AttachTag,
                    start: start as u32,
                    end,
                    expression: expr_json,
                }))
            }
        }
        "render" => {
            parser.allow_whitespace();
            let (expr_json, expr_end) = parser.parse_expression_at(parser.index)?;
            parser.index = expr_end;
            parser.allow_whitespace();
            if !parser.eat("}") {
                return Err(errors::expected_token(
                    Some((parser.index as u32, parser.index as u32)),
                    "}",
                ));
            }
            Ok(FragmentChild::RenderTag(RenderTag {
                kind: RenderTagKind::RenderTag,
                start: start as u32,
                end: parser.index as u32,
                expression: expr_json,
            }))
        }
        "const" | "debug" => Err(errors::expected_token(
            Some((name_start as u32, name_end as u32)),
            "at-tag parsing pending for @{const,debug}",
        )),
        other => Err(errors::expected_token(
            Some((name_start as u32, name_end as u32)),
            &format!("known @-tag, got @{other}"),
        )),
    }
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
    fn plain_identifier_expression() {
        let n = first_node("{foo}");
        match n {
            FragmentChild::ExpressionTag(t) => {
                assert_eq!(t.start, 0);
                assert_eq!(t.end, 5);
                assert_eq!(t.expression["type"], "Identifier");
                assert_eq!(t.expression["name"], "foo");
                assert_eq!(t.expression["start"], 1);
                assert_eq!(t.expression["end"], 4);
            }
            other => panic!("expected ExpressionTag, got {other:?}"),
        }
    }

    #[test]
    fn numeric_literal_expression() {
        let n = first_node("{42}");
        match n {
            FragmentChild::ExpressionTag(t) => {
                assert_eq!(t.expression["type"], "Literal");
                assert_eq!(t.expression["value"], 42);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn expression_with_whitespace() {
        let n = first_node("{ foo }");
        match n {
            FragmentChild::ExpressionTag(t) => {
                assert_eq!(t.start, 0);
                assert_eq!(t.end, 7);
                assert_eq!(t.expression["start"], 2);
                assert_eq!(t.expression["end"], 5);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn html_tag() {
        let n = first_node("{@html foo}");
        match n {
            FragmentChild::HtmlTag(t) => {
                assert_eq!(t.start, 0);
                assert_eq!(t.end, 11);
                assert_eq!(t.expression["type"], "Identifier");
                assert_eq!(t.expression["name"], "foo");
            }
            other => panic!("expected HtmlTag, got {other:?}"),
        }
    }

    #[test]
    fn attach_tag() {
        let n = first_node("{@attach foo()}");
        match n {
            FragmentChild::AttachTag(t) => {
                assert_eq!(t.expression["type"], "CallExpression");
            }
            other => panic!("expected AttachTag, got {other:?}"),
        }
    }

    #[test]
    fn binary_expression() {
        let n = first_node("{a + b}");
        match n {
            FragmentChild::ExpressionTag(t) => {
                assert_eq!(t.expression["type"], "BinaryExpression");
                assert_eq!(t.expression["operator"], "+");
            }
            other => panic!("got {other:?}"),
        }
    }
}
