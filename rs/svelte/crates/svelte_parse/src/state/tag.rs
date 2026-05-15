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
    AttachTag, AttachTagKind, ConstTag, ConstTagKind, DebugTag, DebugTagKind, ExpressionTag,
    ExpressionTagKind, FragmentChild, HtmlTag, HtmlTagKind, RenderTag, RenderTagKind,
};
use svelte_diagnostics::{errors, CompileDiagnostic};

use crate::parser::Parser;
use crate::state::blocks;

/// Find the closing `}` of an at-tag body. Returns the byte offset of the
/// `}` (caller advances past it). Skips over balanced inner braces, strings,
/// and template literals so `{@const x = {a: 1}}` is handled correctly.
fn find_unmatched_brace(template: &str, from: usize) -> Option<usize> {
    let bytes = template.as_bytes();
    let mut depth: i32 = 0;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
                i += 1;
            }
            b @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != b {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    i += 1; // closing quote
                }
            }
            b'`' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'`' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                        // ${expr} — bail to brace-counting branch.
                        i += 2;
                        let mut local_depth = 1;
                        while i < bytes.len() && local_depth > 0 {
                            match bytes[i] {
                                b'{' => local_depth += 1,
                                b'}' => local_depth -= 1,
                                _ => {}
                            }
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    i += 1; // closing backtick
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Recursively shift every numeric `start` / `end` field on the JSON tree
/// by `delta` (positive or negative). Used to relocate an OXC-parsed
/// VariableDeclaration from its synthetic-source coordinates into the
/// template's coordinate space.
fn shift_positions(node: &mut serde_json::Value, delta: i64) {
    match node {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if (key == "start" || key == "end") && v.is_number() {
                    if let Some(n) = v.as_u64() {
                        let shifted = (n as i64).saturating_add(delta).max(0) as u64;
                        *v = serde_json::Value::from(shifted);
                    } else if let Some(n) = v.as_i64() {
                        let shifted = n.saturating_add(delta);
                        *v = serde_json::Value::from(shifted);
                    }
                } else {
                    shift_positions(v, delta);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                shift_positions(v, delta);
            }
        }
        _ => {}
    }
}

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
        "const" => {
            // `{@const NAME = EXPR}` — ported from
            // `phases/1-parse/state/tag.js:674-711`.
            //
            // We parse the slice between `{@const ` and the closing `}`
            // as the synthetic `const NAME = EXPR;` statement via OXC
            // (it's a VariableDeclaration with one declarator). We then
            // wrap it in the upstream wire shape (a Program-like inner
            // VariableDeclaration with `start`/`end` adjusted to point
            // at `const` rather than `@const`).
            parser.allow_whitespace();
            let decl_start = parser.index;
            // Find the closing `}` and parse the content between as
            // `const decl_str;` via OXC's parse_program.
            let close = find_unmatched_brace(parser.template, parser.index).ok_or_else(|| {
                errors::expected_token(
                    Some((decl_start as u32, decl_start as u32)),
                    "}",
                )
            })?;
            let decl_text = parser.template[decl_start..close].trim_end();
            let synthetic = format!("const {decl_text};");
            // Parse as full program to get the VariableDeclaration. We
            // can't use parse_expression_at because `const ...` is a
            // statement, not an expression.
            let (prog, _) = match crate::oxc_bridge::parse_program(
                &synthetic,
                &crate::utils::locator::LineMap::new(&synthetic),
                0,
                synthetic.len(),
                parser.ts,
            ) {
                Ok(v) => v,
                Err(e) => return Err(e),
            };
            // Pull out the first body statement (the VariableDeclaration).
            let body = prog
                .get("body")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .cloned()
                .ok_or_else(|| {
                    errors::expected_token(
                        Some((decl_start as u32, decl_start as u32)),
                        "VariableDeclaration",
                    )
                })?;
            // Shift positions to point into the original template.
            // The synthetic source is `const <decl_text>;`. The user-
            // visible content (`decl_text`) starts at synthetic offset
            // `const `.len() = 6 and runs to synthetic offset
            // 6 + decl_text.len(). In the original template, that span
            // sits at `decl_start..(decl_start + decl_text.len())`.
            // So the shift is `decl_start - 6`.
            let shift: i64 = decl_start as i64 - 6;
            let mut declaration = body;
            shift_positions(&mut declaration, shift);
            // Upstream wraps the declaration's `start` at `decl_start`
            // (the position of `const` itself within the source, not
            // `@const`). element.js:704 says `start: start + 2` — i.e.,
            // skip the `@`. Override.
            if let serde_json::Value::Object(map) = &mut declaration {
                map.insert(
                    "start".to_string(),
                    serde_json::json!(start + 2),
                );
                map.insert("end".to_string(), serde_json::json!(close));
            }
            parser.index = close;
            if !parser.eat("}") {
                return Err(errors::expected_token(
                    Some((parser.index as u32, parser.index as u32)),
                    "}",
                ));
            }
            Ok(FragmentChild::ConstTag(ConstTag {
                kind: ConstTagKind::ConstTag,
                start: start as u32,
                end: parser.index as u32,
                declaration,
            }))
        }
        "debug" => {
            // `{@debug expr1, expr2, ...}` or `{@debug}` — port of
            // `phases/1-parse/state/tag.js:637-672`.
            parser.allow_whitespace();
            if parser.peek() == Some(b'}') {
                parser.index += 1;
                return Ok(FragmentChild::DebugTag(DebugTag {
                    kind: DebugTagKind::DebugTag,
                    start: start as u32,
                    end: parser.index as u32,
                    identifiers: Vec::new(),
                }));
            }
            let (expr, expr_end) = parser.parse_expression_at(parser.index)?;
            parser.index = expr_end;
            parser.allow_whitespace();
            if !parser.eat("}") {
                return Err(errors::expected_token(
                    Some((parser.index as u32, parser.index as u32)),
                    "}",
                ));
            }
            // If the expression is a SequenceExpression, unpack to a list
            // of Identifiers; otherwise the expression itself is the
            // single identifier.
            let identifiers: Vec<serde_json::Value> = if expr.get("type").and_then(|v| v.as_str())
                == Some("SequenceExpression")
            {
                expr.get("expressions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            } else {
                vec![expr]
            };
            // Each must be an Identifier; otherwise emit
            // `debug_tag_invalid_arguments`.
            for id in &identifiers {
                if id.get("type").and_then(|v| v.as_str()) != Some("Identifier") {
                    let pos = id.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    return Err(errors::debug_tag_invalid_arguments(Some((pos, pos))));
                }
            }
            Ok(FragmentChild::DebugTag(DebugTag {
                kind: DebugTagKind::DebugTag,
                start: start as u32,
                end: parser.index as u32,
                identifiers,
            }))
        }
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
