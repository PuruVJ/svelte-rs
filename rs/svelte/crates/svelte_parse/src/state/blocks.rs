//! Block parser — `{#if}`, `{#each}`, `{#await}`, `{#key}`, `{#snippet}`.
//!
//! Ported (partially) from `packages/svelte/src/compiler/phases/1-parse/state/tag.js`
//! (the block-opening branch around lines 70-300 of upstream).
//!
//! Coverage so far:
//! - `{#if expr}...{:else if ...}...{:else}...{/if}` — full `elseif` flattening.
//! - `{#key expr}...{/key}`.
//!
//! Deferred:
//! - `{#each}` (needs Pattern parsing).
//! - `{#await}` (needs Pattern + multi-section parsing).
//! - `{#snippet}` (needs param-list parsing).

use svelte_ast::{
    AwaitBlock,  EachBlock,  Fragment, FragmentChild, 
    IfBlock,  KeyBlock,  Position, SnippetBlock, 
};
use svelte_diagnostics::{errors, CompileDiagnostic};

use crate::oxc_bridge::{parse_expression, parse_pattern_at};
use crate::parser::Parser;
use crate::utils::bracket::{find_matching_bracket, find_matching_pointy};

/// Entry point: caller has consumed `{` and confirmed the next byte is `#`.
/// `start` is the position of the original `{`.
pub fn read_block_open(
    parser: &mut Parser<'_>,
    start: usize,
) -> Result<FragmentChild, CompileDiagnostic> {
    debug_assert_eq!(parser.peek(), Some(b'#'));
    parser.index += 1; // consume `#`

    let name = parser.read_while(|b| b.is_ascii_alphanumeric() || b == b'_');
    let name: String = name.into();

    match name.as_str() {
        "if" => read_if_block(parser, start, false),
        "key" => read_key_block(parser, start),
        "each" => read_each_block(parser, start),
        "snippet" => read_snippet_block(parser, start),
        "await" => read_await_block(parser, start),
        other => Err(errors::expected_token(
            Some((start as u32, parser.index as u32)),
            &format!("known block, got #{other}"),
        )),
    }
}

/// `{#each EXPR as PATTERN[, INDEX][, (KEY)]}...{:else}...{/each}`
///
/// Ported from `phases/1-parse/state/tag.js:81-232`.
///
/// Differences from upstream:
/// - We don't replicate the backtracking-on-failure loop (lines 94-117 of
///   upstream) that strips suffixes from `parser.template` to recover from
///   acorn mistaking `as { y = z }` for an expression. Instead we scan for
///   the top-level ` as ` ourselves so OXC never sees it as part of the
///   each-expression. This covers the fixtures we currently have; the
///   no-context case (`{#each foo, i}`) and TSAsExpression edge cases are
///   left for follow-up.
fn read_each_block(
    parser: &mut Parser<'_>,
    start: usize,
) -> Result<FragmentChild, CompileDiagnostic> {
    parser.allow_whitespace();
    let expr_start = parser.index;

    // Find the top-level ` as ` that separates the iterated expression from
    // the pattern. We scan from the current position, respecting brackets and
    // strings — same approach as match_bracket.
    let as_pos = find_top_level_as(parser.template, expr_start);

    let (expression, context, expr_end_pos) = if let Some(as_pos) = as_pos {
        // Trim trailing whitespace before `as`.
        let mut expr_end = as_pos;
        while expr_end > expr_start {
            let b = parser.template.as_bytes()[expr_end - 1];
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                expr_end -= 1;
            } else {
                break;
            }
        }
        let expression = parse_expression(
            parser.template,
            &parser.line_map,
            expr_start,
            expr_end,
            parser.ts,
        )?;
        parser.index = as_pos + 3; // past `as `
        parser.allow_whitespace();

        // Pattern at the cursor — identifier shortcut or `{...}` / `[...]`
        // via the synthetic-source trick. `read_pattern_with_advance` picks
        // the right path.
        let context = read_pattern_with_advance(parser)?;
        (expression, Some(context), parser.index)
    } else {
        // No `as` — read expression up to a top-level `,` (which separates
        // the iterated expression from the index name) or `}` (end of tag).
        // OXC would otherwise greedy-consume `EXPR, INDEX` as a single
        // SequenceExpression, so bound the slice before handing to OXC.
        let stop_pos = find_top_level_comma_or_close(parser.template, expr_start);
        // Trim trailing whitespace
        let mut expr_end = stop_pos;
        while expr_end > expr_start {
            let b = parser.template.as_bytes()[expr_end - 1];
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                expr_end -= 1;
            } else {
                break;
            }
        }
        let expression = parse_expression(
            parser.template,
            &parser.line_map,
            expr_start,
            expr_end,
            parser.ts,
        )?;
        parser.index = stop_pos;
        (expression, None, stop_pos)
    };

    parser.allow_whitespace();

    let mut index_name: Option<String> = None;
    if parser.eat(",") {
        parser.allow_whitespace();
        let id_start = parser.index;
        let id = parser.read_while(|b| {
            b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
        });
        if id.is_empty() {
            return Err(errors::expected_token(
                Some((id_start as u32, id_start as u32)),
                "identifier",
            ));
        }
        index_name = Some(id.into());
        parser.allow_whitespace();
    }

    let mut key_expr: Option<svelte_js_ast::Expression> = None;
    if parser.eat("(") {
        // `{#each EXPR, i (KEY)}` — key without preceding `as` is invalid.
        // Mirrors `each_key_without_as`.
        if context.is_none() {
            return Err(svelte_diagnostics::errors::each_key_without_as(Some((
                parser.index as u32 - 1,
                parser.index as u32,
            ))));
        }
        parser.allow_whitespace();
        let (k, k_end) = parser.parse_expression_at(parser.index)?;
        parser.index = k_end;
        parser.allow_whitespace();
        if !parser.eat(")") {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                ")",
            ));
        }
        parser.allow_whitespace();
        key_expr = Some(k);
    }

    let _ = expr_end_pos;
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }

    let body = parse_fragment_until_block_boundary(parser, &["else", "/each"])?;

    let fallback = if parser.peek_block_keyword("else") {
        consume_block_continuation(parser, "else")?;
        parser.allow_whitespace();
        if !parser.eat("}") {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                "}",
            ));
        }
        let fb = parse_fragment_until_block_boundary(parser, &["/each"])?;
        Some(fb)
    } else {
        None
    };

    consume_block_close(parser, "each")?;

    Ok(FragmentChild::EachBlock(EachBlock {
        start: start as u32,
        end: parser.index as u32,
        expression,
        body,
        context,
        fallback,
        index: index_name,
        key: key_expr,
    }))
}

/// `{#await EXPR}...{:then [PAT]}...{:catch [PAT]}...{/await}`
/// Also: `{#await EXPR then [PAT]}...{/await}` and `{#await EXPR catch [PAT]}...{/await}`.
///
/// Ported from `phases/1-parse/state/tag.js:235-318`.
fn read_await_block(
    parser: &mut Parser<'_>,
    start: usize,
) -> Result<FragmentChild, CompileDiagnostic> {
    parser.allow_whitespace();
    let expr_start = parser.index;

    // Scan for top-level ` then ` / ` catch ` to bound the expression. If
    // present, the each-style backtracking pattern lets us avoid OXC mistaking
    // it for an identifier.
    let then_inline = find_top_level_keyword(parser.template, expr_start, "then");
    let catch_inline = find_top_level_keyword(parser.template, expr_start, "catch");
    // Choose the earlier of the two if both happen to appear.
    let inline_kw = match (then_inline, catch_inline) {
        (Some(a), Some(b)) if a <= b => Some(("then", a)),
        (Some(_a), Some(b)) => Some(("catch", b)),
        (Some(a), None) => Some(("then", a)),
        (None, Some(b)) => Some(("catch", b)),
        (None, None) => None,
    };

    let (expression, expression_end_in_template) = if let Some((_kw, kw_pos)) = inline_kw {
        let mut expr_end = kw_pos;
        while expr_end > expr_start {
            let b = parser.template.as_bytes()[expr_end - 1];
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                expr_end -= 1;
            } else {
                break;
            }
        }
        let expr = parse_expression(
            parser.template,
            &parser.line_map,
            expr_start,
            expr_end,
            parser.ts,
        )?;
        parser.index = kw_pos;
        (expr, expr_end)
    } else {
        let (expr, end) = parser.parse_expression_at(expr_start)?;
        parser.index = end;
        (expr, end)
    };

    parser.allow_whitespace();

    // The block can have one of:
    //   - just `{#await expr}` (pending fragment first)
    //   - `{#await expr then [pat]}` (then fragment first)
    //   - `{#await expr catch [pat]}` (catch fragment first)
    let mut value: Option<svelte_js_ast::Pattern> = None;
    let mut error: Option<svelte_js_ast::Pattern> = None;
    let mut pending: Option<Fragment> = None;
    let mut then: Option<Fragment> = None;
    let mut catch: Option<Fragment> = None;
    let _ = expression_end_in_template;

    let mut have_then_inline = false;
    let mut have_catch_inline = false;
    if parser.match_str("then") && terminator_after_keyword(parser, "then") {
        parser.index += 4;
        parser.allow_whitespace();
        if parser.peek() != Some(b'}') {
            value = Some(read_pattern_with_advance(parser)?);
            parser.allow_whitespace();
        }
        have_then_inline = true;
    } else if parser.match_str("catch") && terminator_after_keyword(parser, "catch") {
        parser.index += 5;
        parser.allow_whitespace();
        if parser.peek() != Some(b'}') {
            error = Some(read_pattern_with_advance(parser)?);
            parser.allow_whitespace();
        }
        have_catch_inline = true;
    }

    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }

    let mut current = parse_fragment_until_block_boundary(parser, &["then", "catch", "/await"])?;
    if have_then_inline {
        then = Some(current);
    } else if have_catch_inline {
        catch = Some(current);
    } else {
        pending = Some(current);
    }

    // Now any number of `{:then ...}`, `{:catch ...}` sections.
    loop {
        if parser.peek_block_keyword("then") {
            consume_block_continuation(parser, "then")?;
            parser.allow_whitespace();
            if parser.peek() != Some(b'}') {
                value = Some(read_pattern_with_advance(parser)?);
                parser.allow_whitespace();
            }
            if !parser.eat("}") {
                return Err(errors::expected_token(
                    Some((parser.index as u32, parser.index as u32)),
                    "}",
                ));
            }
            current = parse_fragment_until_block_boundary(parser, &["catch", "/await"])?;
            then = Some(current);
        } else if parser.peek_block_keyword("catch") {
            consume_block_continuation(parser, "catch")?;
            parser.allow_whitespace();
            if parser.peek() != Some(b'}') {
                error = Some(read_pattern_with_advance(parser)?);
                parser.allow_whitespace();
            }
            if !parser.eat("}") {
                return Err(errors::expected_token(
                    Some((parser.index as u32, parser.index as u32)),
                    "}",
                ));
            }
            current = parse_fragment_until_block_boundary(parser, &["/await"])?;
            catch = Some(current);
        } else {
            break;
        }
    }

    consume_block_close(parser, "await")?;

    Ok(FragmentChild::AwaitBlock(AwaitBlock {
        start: start as u32,
        end: parser.index as u32,
        expression,
        value,
        error,
        pending,
        then,
        catch_: catch,
    }))
}

/// Check whether the cursor sits on a bare keyword followed by whitespace
/// or `}`. Used to detect inline `then`/`catch` in `{#await expr then ...}`.
fn terminator_after_keyword(parser: &Parser<'_>, keyword: &str) -> bool {
    let after = parser.index + keyword.len();
    matches!(
        parser.template.as_bytes().get(after),
        Some(b' ' | b'\t' | b'\n' | b'\r' | b'}')
    )
}

/// Read a destructuring pattern at the current cursor and advance past it.
///
/// Mirrors upstream `read_pattern` in `read/context.js:12-64`: identifier
/// patterns short-circuit to a hand-built Identifier (so the `loc` uses the
/// original-source line/column via `LineMap`), and only `{...}` / `[...]`
/// patterns go through the synthetic-source `(<pattern> = 1)` trick.
fn read_pattern_with_advance(parser: &mut Parser<'_>) -> Result<svelte_js_ast::Pattern, CompileDiagnostic> {
    let pat_start = parser.index;
    let bytes = parser.template.as_bytes();
    if pat_start >= bytes.len() {
        return Err(errors::expected_token(
            Some((pat_start as u32, pat_start as u32)),
            "destructuring pattern",
        ));
    }

    // Identifier pattern.
    if is_identifier_start(bytes[pat_start]) {
        let mut i = pat_start;
        while i < bytes.len() && is_identifier_continue(bytes[i]) {
            i += 1;
        }
        let pat_end = i;
        let name: String = parser.template[pat_start..pat_end].into();
        // Reject JS reserved words used as `as` patterns. Mirrors
        // `is_reserved` in 1-parse/utils/names.js.
        if is_reserved_word(&name) {
            return Err(errors::unexpected_reserved_word(
                Some((pat_start as u32, pat_start as u32)),
                &name,
            ));
        }
        // `{#each X as $state(...)}` — rune-name pattern is invalid; emit
        // `state_invalid_placement` for the specific case where the
        // identifier is a `$state` / `$derived` / `$props` rune name.
        if matches!(name.as_str(), "$state" | "$derived" | "$props" | "$effect" | "$bindable" | "$inspect" | "$host") {
            let rune = name.clone();
            return Err(svelte_diagnostics::errors::state_invalid_placement(
                Some((pat_start as u32, pat_end as u32)),
                &rune,
            ));
        }
        parser.index = pat_end;
        return Ok(svelte_js_ast::Pattern::Identifier(svelte_js_ast::Identifier {
            name: name.into(),
            span: svelte_js_ast::Span::new(pat_start as u32, pat_end as u32),
        }));
    }

    // `{...}` or `[...]` pattern — synthetic-source trick.
    let pat_end = find_pattern_end(parser.template, pat_start);
    if pat_end <= pat_start {
        return Err(errors::expected_token(
            Some((pat_start as u32, pat_start as u32)),
            "destructuring pattern",
        ));
    }
    let (pat, _) = parse_pattern_at(parser.template, &parser.line_map, pat_start, pat_end, parser.ts)?;
    parser.index = pat_end;
    Ok(pat)
}

fn is_identifier_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        // ES strict-mode reserved words.
        "break" | "case" | "catch" | "class" | "const" | "continue" | "debugger"
        | "default" | "delete" | "do" | "else" | "export" | "extends" | "false"
        | "finally" | "for" | "function" | "if" | "import" | "in" | "instanceof"
        | "new" | "null" | "return" | "super" | "switch" | "this" | "throw"
        | "true" | "try" | "typeof" | "var" | "void" | "while" | "with" | "yield"
        | "enum" | "implements" | "interface" | "package" | "private" | "protected"
        | "public" | "static" | "let"
    )
}

fn is_identifier_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Find the next top-level occurrence of `<space>keyword<space>` in template
/// starting at `from`. Returns the byte offset of the `k` in `keyword`.
fn find_top_level_keyword(template: &str, from: usize, keyword: &str) -> Option<usize> {
    let bytes = template.as_bytes();
    let kw_bytes = keyword.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => i = skip_string_byte(bytes, i + 1, b),
            b'`' => i = skip_template_literal(bytes, i + 1),
            b'(' | b'[' | b'{' => {
                let template_str = std::str::from_utf8(bytes).ok()?;
                let nested_end = find_matching_bracket(template_str, i)?;
                i = nested_end + 1;
            }
            b'}' => return None,
            b' ' | b'\t' | b'\n' | b'\r' => {
                let after = i + 1;
                let end = after + kw_bytes.len();
                if end < bytes.len()
                    && &bytes[after..end] == kw_bytes
                    && matches!(bytes[end], b' ' | b'\t' | b'\n' | b'\r')
                {
                    return Some(after);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// `{#snippet name[<T>](params)}...{/snippet}`
///
/// Ported from `phases/1-parse/state/tag.js:347-419`.
fn read_snippet_block(
    parser: &mut Parser<'_>,
    start: usize,
) -> Result<FragmentChild, CompileDiagnostic> {
    parser.allow_whitespace();

    let id_start = parser.index;
    let id_name_bytes = parser.read_while(|b| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
    });
    if id_name_bytes.is_empty() {
        return Err(errors::expected_identifier(Some((
            id_start as u32,
            id_start as u32,
        ))));
    }
    let id_name: String = id_name_bytes.into();
    let id_end = parser.index;
    let expression = svelte_js_ast::Identifier {
        name: id_name.into(),
        span: svelte_js_ast::Span::new(id_start as u32, id_end as u32),
    };

    parser.allow_whitespace();

    // TypeScript generic parameters: `<T extends ...>`
    let mut type_params: Option<String> = None;
    if parser.ts && parser.peek() == Some(b'<') {
        let lt_pos = parser.index;
        match find_matching_pointy(parser.template, lt_pos) {
            Some(gt_pos) => {
                // Slice between `<` and `>` (exclusive on both).
                let tp: String = parser.template[lt_pos + 1..gt_pos].into();
                type_params = Some(tp);
                parser.index = gt_pos + 1;
            }
            None => {
                return Err(errors::expected_token(
                    Some((lt_pos as u32, lt_pos as u32)),
                    ">",
                ));
            }
        }
    }

    parser.allow_whitespace();

    // Parameter list: `(...params...)`. Must be present.
    let params_start = parser.index;
    let params_end = if parser.peek() == Some(b'(') {
        match find_matching_bracket(parser.template, params_start) {
            Some(rp) => {
                parser.index = rp + 1;
                rp + 1
            }
            None => {
                return Err(errors::expected_token(
                    Some((params_start as u32, params_start as u32)),
                    ")",
                ));
            }
        }
    } else {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "(",
        ));
    };

    let (parameters, _) = crate::oxc_bridge::parse_arrow_params_at(
        parser.template,
        &parser.line_map,
        params_start,
        params_end,
        parser.ts,
    )?;

    parser.allow_whitespace();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }

    let body = parse_fragment_until_block_boundary(parser, &["/snippet"])?;
    consume_block_close(parser, "snippet")?;

    Ok(FragmentChild::SnippetBlock(SnippetBlock {
        start: start as u32,
        end: parser.index as u32,
        expression,
        parameters,
        type_params,
        body,
    }))
}

fn position_to_json(p: &Position) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("line".to_string(), serde_json::json!(p.line));
    o.insert("column".to_string(), serde_json::json!(p.column));
    if let Some(c) = p.character {
        o.insert("character".to_string(), serde_json::json!(c));
    }
    serde_json::Value::Object(o)
}

/// Find the byte offset of the next top-level ` as ` keyword starting at
/// `from`. Top-level means depth 0 with respect to `()`, `[]`, `{}`, and
/// outside any string / template literal. Returns the offset of the `a` in
/// `as` (the leading space is implicit).
fn find_top_level_as(template: &str, from: usize) -> Option<usize> {
    let bytes = template.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => i = skip_string_byte(bytes, i + 1, b),
            b'`' => i = skip_template_literal(bytes, i + 1),
            b'(' | b'[' | b'{' => {
                let template_str = std::str::from_utf8(bytes).ok()?;
                let nested_end = find_matching_bracket(template_str, i)?;
                i = nested_end + 1;
            }
            b'}' => return None, // hit the end of the block opening
            b' ' | b'\t' | b'\n' | b'\r' => {
                // check for ` as `
                let after = i + 1;
                if after + 2 < bytes.len()
                    && bytes[after] == b'a'
                    && bytes[after + 1] == b's'
                    && matches!(bytes[after + 2], b' ' | b'\t' | b'\n' | b'\r')
                {
                    return Some(after);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Find the position of the first top-level `,` or `}` (whichever comes
/// first) starting from `from`. Used by the no-`as` each-block branch to
/// bound the iterated expression before handing it to OXC.
fn find_top_level_comma_or_close(template: &str, from: usize) -> usize {
    let bytes = template.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => i = skip_string_byte(bytes, i + 1, b),
            b'`' => i = skip_template_literal(bytes, i + 1),
            b'(' | b'[' | b'{' => {
                let template_str = match std::str::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => return i,
                };
                let nested_end = match find_matching_bracket(template_str, i) {
                    Some(e) => e,
                    None => return i,
                };
                i = nested_end + 1;
            }
            b',' | b'}' => return i,
            _ => i += 1,
        }
    }
    i
}

/// Find where a destructuring pattern ends. Handles identifier names and
/// `{...}` / `[...]` openers (delegating bracket matching to
/// `find_matching_bracket`).
fn find_pattern_end(template: &str, start: usize) -> usize {
    let bytes = template.as_bytes();
    if start >= bytes.len() {
        return start;
    }
    match bytes[start] {
        b'{' | b'[' => find_matching_bracket(template, start)
            .map(|e| e + 1)
            .unwrap_or(start),
        _ => {
            // identifier characters (loose — JS supports `$`, `_`, digits
            // after the first char). We don't validate the first char; the
            // caller is the one who decides we're in pattern territory.
            let mut i = start;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
            {
                i += 1;
            }
            i
        }
    }
}

fn skip_string_byte(bytes: &[u8], mut i: usize, quote: u8) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

fn skip_template_literal(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
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

/// `{#if expr}...{:else if ...}...{:else}...{/if}`
///
/// `elseif` is `true` when this IfBlock was reached via `{:else if}` — in
/// that case its `start` is the position of the `{` in `{:else if`, and the
/// recursive chain is responsible for eventually consuming the single
/// shared `{/if}` at the end. Both the outermost and every nested elseif
/// IfBlock end at the same position (just past `{/if}`).
fn read_if_block(
    parser: &mut Parser<'_>,
    start: usize,
    elseif: bool,
) -> Result<FragmentChild, CompileDiagnostic> {
    parser.allow_whitespace();
    let (test, expr_end) = parser.parse_expression_at(parser.index)?;
    parser.index = expr_end;
    parser.allow_whitespace();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }

    // Parse consequent until `{:else}`, `{:else if}` or `{/if}`.
    let consequent = parse_fragment_until_block_boundary(parser, &["else", "/if"])?;

    let alternate = if parser.peek_block_keyword("else") {
        // Capture the `{` position BEFORE consuming — the nested elseif
        // IfBlock's `start` is this brace position.
        let else_brace_pos = parser.index;
        consume_block_continuation(parser, "else")?;
        parser.allow_whitespace();
        if parser.match_str("if ") || parser.match_str("if\t") || parser.match_str("if\n") {
            // `{:else if ...}` — flatten as a nested elseif IfBlock.
            // The recursive call will eventually consume the shared `{/if}`.
            parser.index += 2;
            let nested = read_if_block(parser, else_brace_pos, true)?;
            Some(Fragment {
                nodes: vec![nested],
            })
        } else {
            // `{:else}` — eat `}` then parse the else body up to `{/if}`.
            if !parser.eat("}") {
                return Err(errors::expected_token(
                    Some((parser.index as u32, parser.index as u32)),
                    "}",
                ));
            }
            let alt = parse_fragment_until_block_boundary(parser, &["/if"])?;
            consume_block_close(parser, "if")?;
            Some(alt)
        }
    } else {
        // No `:else` — directly `{/if}`.
        consume_block_close(parser, "if")?;
        None
    };

    let _ = elseif; // kept for the field on the AST; no longer affects close-consumption.
    Ok(FragmentChild::IfBlock(IfBlock {
        start: start as u32,
        end: parser.index as u32,
        elseif,
        test,
        consequent,
        alternate,
    }))
}

/// `{#key expr}...{/key}`
fn read_key_block(
    parser: &mut Parser<'_>,
    start: usize,
) -> Result<FragmentChild, CompileDiagnostic> {
    parser.allow_whitespace();
    let (expression, expr_end) = parser.parse_expression_at(parser.index)?;
    parser.index = expr_end;
    parser.allow_whitespace();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }

    let fragment = parse_fragment_until_block_boundary(parser, &["/key"])?;
    consume_block_close(parser, "key")?;

    Ok(FragmentChild::KeyBlock(KeyBlock {
        start: start as u32,
        end: parser.index as u32,
        expression,
        fragment,
    }))
}

/// Parse children until the parser cursor sits at `{:keyword}` (for any
/// keyword in `boundaries`) or `{/keyword}`. Does NOT consume the boundary
/// marker; that's the caller's job.
fn parse_fragment_until_block_boundary(
    parser: &mut Parser<'_>,
    boundaries: &[&str],
) -> Result<Fragment, CompileDiagnostic> {
    let mut nodes: Vec<FragmentChild> = Vec::with_capacity(8);
    loop {
        if parser.index >= parser.template.len() {
            // Find the position of the enclosing block-open `{#...`. The
            // outer caller passes us boundaries containing the close-tag
            // keyword like `/if`; emit `block_unclosed` with the start of
            // the open tag as a guess (we don't track it here, so use 0).
            return Err(svelte_diagnostics::errors::block_unclosed(Some((
                0, 1,
            ))));
        }

        if parser.match_str("{") && peek_is_boundary(parser, boundaries) {
            break;
        }

        if parser.match_str("<!--") {
            let c = super::comment::read_comment(parser)?;
            nodes.push(FragmentChild::Comment(c));
            continue;
        }
        if parser.match_str("<") {
            let child = super::element::read_element_or_comment(parser)?;
            nodes.push(child);
            continue;
        }
        if parser.match_str("{") {
            nodes.push(super::tag::read_tag(parser)?);
            continue;
        }
        let t = super::text::read_text(parser);
        nodes.push(FragmentChild::Text(t));
    }
    Ok(Fragment {
        nodes,
    })
}

/// Peek: is the cursor on `{:KW` or `{/KW` for any keyword in `boundaries`?
/// `boundaries` entries that start with `/` are close-tag matches.
fn peek_is_boundary(parser: &Parser<'_>, boundaries: &[&str]) -> bool {
    let after = match parser.template[parser.index..].strip_prefix('{') {
        Some(s) => s,
        None => return false,
    };
    // skip whitespace after `{`
    let after = after.trim_start();
    for kw in boundaries {
        if let Some(stripped) = kw.strip_prefix('/') {
            // `{/foo}` close-tag.
            let candidate = format!("/{stripped}");
            if after.starts_with(&candidate)
                && terminator_ok_after(&after[candidate.len()..], true)
            {
                return true;
            }
        } else {
            // `{:foo}` continuation.
            let candidate = format!(":{kw}");
            if after.starts_with(&candidate)
                && terminator_ok_after(&after[candidate.len()..], false)
            {
                return true;
            }
        }
    }
    false
}

/// After a candidate keyword, the next character must mark the end of the
/// keyword token (whitespace, `}`, or for continuations also another letter
/// — e.g. `{:else if}` has `else` followed by ` if`).
fn terminator_ok_after(rest: &str, is_close: bool) -> bool {
    let _ = is_close;
    if rest.is_empty() {
        return true;
    }
    let b = rest.as_bytes()[0];
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'}')
}

/// Consume `{:keyword` (caller will handle whitespace and `}` separately).
fn consume_block_continuation(
    parser: &mut Parser<'_>,
    keyword: &str,
) -> Result<(), CompileDiagnostic> {
    if !parser.eat("{") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "{",
        ));
    }
    parser.allow_whitespace();
    if !parser.eat(":") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            ":",
        ));
    }
    if !parser.eat(keyword) {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            keyword,
        ));
    }
    Ok(())
}

/// Consume `{/keyword}`.
fn consume_block_close(
    parser: &mut Parser<'_>,
    keyword: &str,
) -> Result<(), CompileDiagnostic> {
    if !parser.eat("{") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "{",
        ));
    }
    parser.allow_whitespace();
    if !parser.eat("/") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "/",
        ));
    }
    if !parser.eat(keyword) {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            keyword,
        ));
    }
    parser.allow_whitespace();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }
    Ok(())
}

impl<'a> Parser<'a> {
    /// Cheap predicate: does the cursor sit on `{:kw` or `{ :kw` (with
    /// allowed whitespace)?
    pub fn peek_block_keyword(&self, kw: &str) -> bool {
        let after = match self.template[self.index..].strip_prefix('{') {
            Some(s) => s.trim_start(),
            None => return false,
        };
        let candidate = format!(":{kw}");
        after.starts_with(&candidate)
            && terminator_ok_after(&after[candidate.len()..], false)
    }
}

