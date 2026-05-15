//! Bridge from OXC's JS/TS AST to the acorn-shaped estree JSON that Svelte's
//! parser output consumes.
//!
//! Background: Svelte's compiler uses `acorn` + `@sveltejs/acorn-typescript`
//! to parse `<script>` blocks and `{expression}` mustaches. The resulting
//! AST is JSON with `type`, `start`, `end`, and `loc: { start: {line, col},
//! end: {line, col} }` on every node. We replace acorn with OXC; this module
//! is the adapter that makes OXC's output look like acorn's.
//!
//! Differences this adapter reconciles:
//! - OXC emits `range: [start, end]` arrays by default; we disable those.
//! - OXC does NOT emit `loc` (line/col); we post-process and inject them.
//! - OXC positions are relative to the source slice it parsed; we shift them
//!   into the original `.svelte` file's coordinate space.

use oxc_allocator::Allocator;
use oxc_ast::ast::CommentKind;
use oxc_estree::{ESTree, PrettyTSSerializer};
use oxc_parser::{ParseOptions, Parser as OxcParser};
use oxc_span::{GetSpan, SourceType};
use serde_json::Value;
use svelte_diagnostics::{errors, CompileDiagnostic};

use crate::utils::locator::LineMap;

/// A JS comment captured during expression / program parsing. Mirrors the
/// shape acorn pushes through its `onComment` callback —
/// `{ type, value, start, end }`. The `loc` field is added later (in the
/// places that need it, like `Root.comments`).
#[derive(Debug, Clone)]
pub struct RawComment {
    pub line: bool,   // true = Line, false = Block
    pub start: u32,
    pub end: u32,
    pub value: String,
    /// When true, the wire format must include `loc.{start,end}.character`.
    /// Mirrors the difference between acorn `onComment` callbacks (no
    /// character) and `read_comment` in element.js (uses `locator()` which
    /// returns `{line,column,character}`).
    pub with_character: bool,
}

/// Parse a JS/TS expression from `source[start..end]` and return its
/// estree-shaped JSON with positions shifted into `source`'s coordinate space
/// and `loc` fields populated from `line_map`.
///
/// Caller is expected to have already located the slice boundaries — for
/// `{foo}`, that's `start = offset_after_{`, `end = offset_before_}`.
pub fn parse_expression(
    full_source: &str,
    line_map: &LineMap,
    start: usize,
    end: usize,
    ts: bool,
) -> Result<Value, CompileDiagnostic> {
    let slice = &full_source[start..end];
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts);

    let parser = OxcParser::new(&allocator, slice, source_type)
        .with_options(svelte_compatible_options());
    let expression = match parser.parse_expression() {
        Ok(e) => e,
        Err(diags) => {
            let msg = diags
                .iter()
                .map(|d| format!("{d}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(errors::js_parse_error(
                Some((start as u32, end as u32)),
                &msg,
            ));
        }
    };

    let mut s = PrettyTSSerializer::new(false);
    expression.serialize(&mut s);
    let raw_json = s.into_string();

    let mut value: Value = serde_json::from_str(&raw_json)
        .map_err(|e| errors::js_parse_error(Some((start as u32, end as u32)), &e.to_string()))?;

    normalize_template_elements(&mut value);
    shift_and_locate(&mut value, start, line_map);
    unwrap_parens(&mut value);
    strip_empty_ts_defaults(&mut value);
    Ok(value)
}

/// Options for the OXC pre-parse pass. We keep parens in the AST during
/// parsing so OXC's `span()` correctly reports the end of `(expr)` as the
/// position past `)` (the caller relies on this to know where the cursor
/// advances to). We then post-process the JSON to unwrap `ParenthesizedExpression`
/// nodes — acorn's default is `preserveParens: false`, and Svelte calls it
/// without overriding that.
fn svelte_compatible_options() -> ParseOptions {
    ParseOptions {
        preserve_parens: true,
        ..ParseOptions::default()
    }
}

/// Recursively strip fields that OXC emits as empty defaults but
/// `@sveltejs/acorn-typescript` omits when not relevant. Matches the
/// upstream wire format on a per-field basis.
///
/// Fields stripped:
/// - `decorators: []` — TS decorators; absent when no decorators.
/// - `optional: false` — TS optional marker; absent for non-optional.
/// - `typeAnnotation: null` — TS type annotation; absent when none. Note:
///   we keep non-null values.
fn strip_empty_ts_defaults(node: &mut Value) {
    match node {
        Value::Object(map) => {
            // Inspect values first so we strip from descendants too.
            for (_, v) in map.iter_mut() {
                strip_empty_ts_defaults(v);
            }
            // Now check this object's fields.
            let strip_decorators = map
                .get("decorators")
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty());
            if strip_decorators {
                map.remove("decorators");
            }
            // `optional: false` — acorn emits this on `CallExpression` and
            // `MemberExpression` as the optional-chaining (`?.`) marker. On
            // all other node kinds (TS Identifier params, TSPropertySignature,
            // etc.) acorn-typescript only emits it when truthy. Strip when
            // false unless this is a node where acorn unconditionally emits.
            let keep_optional = matches!(
                map.get("type").and_then(|v| v.as_str()),
                Some("CallExpression") | Some("MemberExpression")
            );
            if !keep_optional && map.get("optional") == Some(&Value::Bool(false)) {
                map.remove("optional");
            }
            if map.get("typeAnnotation") == Some(&Value::Null) {
                map.remove("typeAnnotation");
            }
            // OXC emits `directive: null` on ExpressionStatement; acorn omits.
            if map.get("directive") == Some(&Value::Null) {
                map.remove("directive");
            }
            // TS-only flags that acorn-typescript only emits when truthy.
            if map.get("definite") == Some(&Value::Bool(false)) {
                map.remove("definite");
            }
            if map.get("declare") == Some(&Value::Bool(false)) {
                map.remove("declare");
            }
            // OXC adds `hashbang: null` on Program; acorn omits.
            if map.get("hashbang") == Some(&Value::Null) {
                map.remove("hashbang");
            }
            // OXC adds `accessibility: null` and `override: false` on class
            // members; acorn-typescript only emits these when meaningful.
            if map.get("accessibility") == Some(&Value::Null) {
                map.remove("accessibility");
            }
            if map.get("override") == Some(&Value::Bool(false)) {
                map.remove("override");
            }
            if map.get("readonly") == Some(&Value::Bool(false)) {
                map.remove("readonly");
            }
            if map.get("abstract") == Some(&Value::Bool(false)) {
                map.remove("abstract");
            }
            // TS function-related empty defaults.
            if map.get("returnType") == Some(&Value::Null) {
                map.remove("returnType");
            }
            if map.get("typeParameters") == Some(&Value::Null) {
                map.remove("typeParameters");
            }
            if map.get("typeArguments") == Some(&Value::Null) {
                map.remove("typeArguments");
            }
            // OXC's BlockStatement has `body: []` always populated — same as acorn.
            // OXC may emit `superClass: null` on Class — acorn omits.
            if map.get("superClass") == Some(&Value::Null) {
                map.remove("superClass");
            }
            // OXC emits `value: null` on RestElement (object/array rest in
            // destructuring patterns); acorn-typescript omits it.
            if map.get("type").and_then(|v| v.as_str()) == Some("RestElement")
                && map.get("value") == Some(&Value::Null)
            {
                map.remove("value");
            }
            // `exportKind: "value"` and `importKind: "value"` are TS defaults
            // (vs "type"); acorn-typescript only emits them when "type".
            if map.get("exportKind") == Some(&Value::String("value".into())) {
                map.remove("exportKind");
            }
            if map.get("importKind") == Some(&Value::String("value".into())) {
                map.remove("importKind");
            }
            // OXC adds `phase: null` on ImportExpression — acorn omits.
            if map.get("phase") == Some(&Value::Null) {
                map.remove("phase");
            }
            // `expression: false` on ArrowFunctionExpression (per acorn) is
            // emitted only when relevant; OXC always emits it.
            // Actually acorn-typescript DOES emit `expression: bool` always.
            // Don't strip — leaving as-is to test fixtures.
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_empty_ts_defaults(v);
            }
        }
        _ => {}
    }
}

/// OXC reports TemplateElement bounds inclusive of the surrounding
/// backticks / `${` / `}` markers; acorn reports just the inner content.
/// Adjust each `TemplateElement` in-place so its `start`/`end` match acorn:
///
/// - `start += 1` (skip the leading `` ` `` or `}`)
/// - `end -= 2` if NOT a tail (skip trailing `${`)
/// - `end -= 1` if a tail (skip trailing `` ` ``)
fn normalize_template_elements(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("TemplateElement") {
                let tail = map
                    .get("tail")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(s) = map.get("start").and_then(|v| v.as_u64()) {
                    map.insert("start".to_string(), Value::from(s.saturating_add(1)));
                }
                if let Some(e) = map.get("end").and_then(|v| v.as_u64()) {
                    let adj = if tail { 1 } else { 2 };
                    map.insert("end".to_string(), Value::from(e.saturating_sub(adj)));
                }
            }
            for (_, v) in map.iter_mut() {
                normalize_template_elements(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_template_elements(v);
            }
        }
        _ => {}
    }
}

/// Recursively replace every `{"type": "ParenthesizedExpression", "expression": X}`
/// node with `X`. Matches acorn's default behavior.
fn unwrap_parens(node: &mut Value) {
    match node {
        Value::Object(map) => {
            // Unwrap children first.
            for (_, v) in map.iter_mut() {
                unwrap_parens(v);
            }
            // Then unwrap this node if it's a ParenthesizedExpression.
            if map.get("type").and_then(|v| v.as_str()) == Some("ParenthesizedExpression") {
                if let Some(inner) = map.remove("expression") {
                    *node = inner;
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                unwrap_parens(v);
            }
        }
        _ => {}
    }
}

/// Parse a full ECMAScript/TypeScript module starting at `start..end` in
/// `full_source` and return its `Program` AST as estree-shaped JSON, with
/// positions shifted into `full_source`'s coordinate space, `loc` fields
/// populated, and `leadingComments` attached to nodes from the parser's
/// comment list.
///
/// Used by `<script>` body parsing in Phase 2f. Comments that aren't
/// attached to an inner node (e.g. trailing) are returned in the second
/// tuple element for the caller to use (e.g. populating `Root.comments`).
pub fn parse_program(
    full_source: &str,
    line_map: &LineMap,
    start: usize,
    end: usize,
    ts: bool,
) -> Result<(Value, Vec<RawComment>), CompileDiagnostic> {
    let slice = &full_source[start..end];
    let allocator = Allocator::default();
    let source_type = SourceType::default()
        .with_typescript(ts)
        .with_module(true);

    let parser = OxcParser::new(&allocator, slice, source_type)
        .with_options(svelte_compatible_options());
    let ret = parser.parse();
    if !ret.errors.is_empty() {
        let msg = ret
            .errors
            .iter()
            .map(|d| format!("{d}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(errors::js_parse_error(
            Some((start as u32, end as u32)),
            &msg,
        ));
    }

    // Collect comments BEFORE serializing (comments live on the Program but
    // aren't emitted by the ESTree serializer).
    let raw_comments: Vec<RawComment> = ret
        .program
        .comments
        .iter()
        .map(|c| RawComment {
            line: matches!(c.kind, CommentKind::Line),
            start: c.span.start + start as u32,
            end: c.span.end + start as u32,
            value: comment_value(
                full_source,
                c.span.start as usize + start,
                c.span.end as usize + start,
                c.kind,
            ),
            with_character: false,
        })
        .collect();

    let mut s = PrettyTSSerializer::new(false);
    ret.program.serialize(&mut s);
    let raw_json = s.into_string();

    let mut value: Value = serde_json::from_str(&raw_json)
        .map_err(|e| errors::js_parse_error(Some((start as u32, end as u32)), &e.to_string()))?;

    normalize_template_elements(&mut value);
    shift_and_locate(&mut value, start, line_map);
    unwrap_parens(&mut value);
    strip_empty_ts_defaults(&mut value);
    attach_leading_comments(&mut value, &raw_comments);
    Ok((value, raw_comments))
}

/// Extract the textual content of a comment from the original source,
/// stripping delimiters.
fn comment_value(source: &str, start: usize, end: usize, kind: CommentKind) -> String {
    let raw = &source[start..end];
    match kind {
        // `//content` → strip the leading `//`
        CommentKind::Line => raw.strip_prefix("//").unwrap_or(raw).to_string(),
        // `/*content*/` → strip both delimiters
        CommentKind::SingleLineBlock | CommentKind::MultiLineBlock => raw
            .strip_prefix("/*")
            .and_then(|s| s.strip_suffix("*/"))
            .unwrap_or(raw)
            .to_string(),
    }
}

/// Attach each `RawComment` as a `leadingComments` entry on the appropriate
/// AST node. Acorn-style rule:
///
/// - Comments *before the first statement* in a Program attach to Program.
/// - Comments between two statements attach to the second statement.
/// - Comments inside expressions attach to the deepest expression node
///   that starts at-or-after the comment.
///
/// We implement this by walking the AST top-down: at each node, partition
/// comments by where they fall relative to the node's children. Anything
/// not fitting a deeper child becomes a leadingComment on that node.
fn attach_leading_comments(program: &mut Value, comments: &[RawComment]) {
    if comments.is_empty() {
        return;
    }
    let Value::Object(prog) = program else { return };
    let prog_start = prog.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let _ = prog_start;

    // Determine first statement's start (or None if Program is empty).
    let first_body_start: Option<u32> = prog
        .get("body")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("start"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let body_arr_len = prog
        .get("body")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let _ = body_arr_len;

    // For each comment, decide whether it attaches to Program or to an
    // inner statement.
    for comment in comments {
        match first_body_start {
            None => {
                // No statements — acorn attaches as trailingComments on
                // Program. Mirrors how `onComment` accumulates and acorn
                // promotes them when no statement is available to host.
                push_trailing_comment(prog, comment);
            }
            Some(first_start) if comment.end <= first_start => {
                // Before the first statement — attach to Program.
                if attach_inside_body(prog, comment, true) {
                    continue;
                }
                push_leading_comment(prog, comment);
            }
            _ => {
                // Inside or between statements — attach to the next statement
                // or to a deeper inner expression node.
                attach_inside_body(prog, comment, false);
            }
        }
    }
}

fn push_trailing_comment(node: &mut serde_json::Map<String, Value>, comment: &RawComment) {
    let entry = serde_json::json!({
        "type": if comment.line { "Line" } else { "Block" },
        "value": comment.value,
        "start": comment.start,
        "end": comment.end,
    });
    let list = node
        .entry("trailingComments".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(arr) = list {
        arr.push(entry);
    }
}

/// Walks Program.body to find the right statement to attach a comment to.
/// `top_level_before_first` is `true` when the comment is positioned before
/// the first statement (in which case `attach_inside_body` returns `false`
/// and the caller attaches to Program).
fn attach_inside_body(
    prog: &mut serde_json::Map<String, Value>,
    comment: &RawComment,
    top_level_before_first: bool,
) -> bool {
    if top_level_before_first {
        return false;
    }
    let body = match prog.get_mut("body").and_then(|v| v.as_array_mut()) {
        Some(b) => b,
        None => return false,
    };
    // Find the first statement whose start > comment.end. Attach there.
    for stmt in body.iter_mut() {
        if let Some(s) = stmt.get("start").and_then(|v| v.as_u64()) {
            if (s as u32) > comment.end {
                if let Value::Object(m) = stmt {
                    push_leading_comment(m, comment);
                    return true;
                }
            }
        }
    }
    false
}

fn push_leading_comment(map: &mut serde_json::Map<String, Value>, comment: &RawComment) {
    let entry = serde_json::json!({
        "type": if comment.line { "Line" } else { "Block" },
        "value": comment.value,
    });
    let leading = map
        .entry("leadingComments".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(arr) = leading {
        arr.push(entry);
    }
}

/// Parse one expression starting at `start` in `full_source` and continue
/// reading until OXC decides the expression has ended (i.e. it encounters
/// a token that can't extend the expression — typically `}` for our mustache
/// use). Returns the expression JSON (with shifted positions and `loc`
/// fields) and the absolute end offset just past the expression.
///
/// This is the mustache-friendly variant: callers don't need to find the
/// matching `}` themselves; OXC does that as part of expression parsing.
pub fn parse_expression_at(
    full_source: &str,
    line_map: &LineMap,
    start: usize,
    ts: bool,
) -> Result<(Value, usize), CompileDiagnostic> {
    parse_expression_at_with_comments(full_source, line_map, start, ts).map(|(v, e, _)| (v, e))
}

/// Like `parse_expression_at`, but additionally returns any comments OXC
/// collected during parsing (with positions shifted into `full_source`'s
/// coordinate space). Used by the top-level `parse` to populate
/// `Root.comments`.
pub fn parse_expression_at_with_comments(
    full_source: &str,
    line_map: &LineMap,
    start: usize,
    ts: bool,
) -> Result<(Value, usize, Vec<RawComment>), CompileDiagnostic> {
    let slice = &full_source[start..];
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts);

    // First pass: parse_expression stops at the end of one expression.
    let parser = OxcParser::new(&allocator, slice, source_type)
        .with_options(svelte_compatible_options());
    let expression = match parser.parse_expression() {
        Ok(e) => e,
        Err(diags) => {
            let msg = diags
                .iter()
                .map(|d| format!("{d}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(errors::js_parse_error(
                Some((start as u32, start as u32)),
                &msg,
            ));
        }
    };

    let span = expression.span();
    let abs_end = start + span.end as usize;

    // Second pass (just for comments): parse the slice up to span.end as a
    // full program — that gives us access to `program.comments`.
    // `parse_expression` doesn't expose collected comments directly.
    let raw_comments = collect_comments_for_slice(&allocator, &slice[..span.end as usize], ts)
        .into_iter()
        .map(|c| RawComment {
            line: c.line,
            start: c.start + start as u32,
            end: c.end + start as u32,
            value: comment_value(
                full_source,
                c.start as usize + start,
                c.end as usize + start,
                if c.line {
                    CommentKind::Line
                } else {
                    CommentKind::MultiLineBlock
                },
            ),
            with_character: false,
        })
        .collect::<Vec<_>>();

    let mut s = PrettyTSSerializer::new(false);
    expression.serialize(&mut s);
    let raw_json = s.into_string();

    let mut value: Value = serde_json::from_str(&raw_json)
        .map_err(|e| errors::js_parse_error(Some((start as u32, abs_end as u32)), &e.to_string()))?;

    normalize_template_elements(&mut value);
    shift_and_locate(&mut value, start, line_map);
    unwrap_parens(&mut value);
    strip_empty_ts_defaults(&mut value);
    attach_leading_comments_to_expression(&mut value, &raw_comments);
    Ok((value, abs_end, raw_comments))
}

/// Parse a destructuring pattern at `pattern_start..pattern_end` and return
/// its acorn-shape JSON. Mirrors `read_pattern` in
/// `packages/svelte/src/compiler/phases/1-parse/read/context.js`.
///
/// Patterns themselves aren't valid expressions (e.g. `{ name = true }` is a
/// SyntaxError), so we replicate upstream's trick: build a synthetic source
/// where the original prefix is replaced with whitespace (newlines preserved),
/// the first space is removed to make room for an inserted `(`, and the
/// pattern is wrapped as `(<pattern> = 1)`. Parsing that yields an
/// AssignmentExpression whose `left` is the pattern, and the positions
/// (start/end + line/column) already match the original template — the
/// removed-space hack keeps byte offsets identical while column-on-line-1
/// stays aligned (columns on subsequent lines end up +1 vs the original;
/// that's upstream's behavior, so we preserve it byte-for-byte).
///
/// Note that the `_line_map` argument is intentionally unused — we build a
/// fresh LineMap from the synthetic source so line/column queries match what
/// acorn would have reported.
pub fn parse_pattern_at(
    full_source: &str,
    _line_map: &LineMap,
    pattern_start: usize,
    pattern_end: usize,
    ts: bool,
) -> Result<Value, CompileDiagnostic> {
    let pattern_slice = &full_source[pattern_start..pattern_end];

    // Build the whitespace-padded prefix (newlines preserved, other chars → space).
    let mut padded_prefix: Vec<u8> = full_source.as_bytes()[..pattern_start]
        .iter()
        .map(|b| if *b == b'\n' { b'\n' } else { b' ' })
        .collect();
    // Remove first space so that prepending `(` leaves the pattern at byte
    // `pattern_start` in the synthetic source.
    if let Some(first_space_idx) = padded_prefix.iter().position(|&b| b == b' ') {
        padded_prefix.remove(first_space_idx);
    } else {
        // No space to remove — pattern is at byte 0 (no prefix). Just don't
        // pad.
    }
    let prefix_str = std::str::from_utf8(&padded_prefix).map_err(|_| {
        errors::js_parse_error(Some((pattern_start as u32, pattern_end as u32)), "non-utf8")
    })?;
    let synthetic = format!("{prefix_str}({pattern_slice} = 1)");

    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts);
    let parser = OxcParser::new(&allocator, &synthetic, source_type)
        .with_options(svelte_compatible_options());
    let expression = match parser.parse_expression() {
        Ok(e) => e,
        Err(diags) => {
            let msg = diags
                .iter()
                .map(|d| format!("{d}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(errors::js_parse_error(
                Some((pattern_start as u32, pattern_end as u32)),
                &msg,
            ));
        }
    };

    let mut s = PrettyTSSerializer::new(false);
    expression.serialize(&mut s);
    let raw_json = s.into_string();
    let mut value: Value = serde_json::from_str(&raw_json).map_err(|e| {
        errors::js_parse_error(
            Some((pattern_start as u32, pattern_end as u32)),
            &e.to_string(),
        )
    })?;

    // OXC reports positions relative to the synthetic source (which is mostly
    // whitespace then `(<pattern> = 1)`). Build a LineMap on the synthetic
    // source so injected `loc` fields reflect the same line/column as JS.
    let synthetic_line_map = LineMap::new(&synthetic);
    normalize_template_elements(&mut value);
    shift_and_locate(&mut value, 0, &synthetic_line_map);
    unwrap_parens(&mut value);
    strip_empty_ts_defaults(&mut value);

    // After unwrapping parens, the top-level node is the AssignmentExpression.
    // The pattern is its `left` field.
    let Some(map) = value.as_object_mut() else {
        return Err(errors::js_parse_error(
            Some((pattern_start as u32, pattern_end as u32)),
            "expected AssignmentExpression at wrapped-pattern root",
        ));
    };
    match map.remove("left") {
        Some(left) => Ok(left),
        None => Err(errors::js_parse_error(
            Some((pattern_start as u32, pattern_end as u32)),
            "wrapped-pattern AssignmentExpression has no `left`",
        )),
    }
}

/// Parse the parameter list of `{#snippet name(<params>)}` and return the
/// `parameters` array of an ArrowFunctionExpression. Mirrors the upstream
/// snippet-parameter parsing path at `tag.js:390-397`.
///
/// `params_start` is the position of the opening `(`; `params_end` is just
/// after the closing `)`. Like `parse_pattern_at`, we pad the original prefix
/// with whitespace (newlines preserved) so OXC reports line/column matching
/// what acorn would emit for the same input.
pub fn parse_arrow_params_at(
    full_source: &str,
    _line_map: &LineMap,
    params_start: usize,
    params_end: usize,
    ts: bool,
) -> Result<Vec<Value>, CompileDiagnostic> {
    let params_slice = &full_source[params_start..params_end];
    // Pad prefix with spaces / newlines so the params are at the same
    // template position in the synthetic source.
    let padded_prefix: Vec<u8> = full_source.as_bytes()[..params_start]
        .iter()
        .map(|b| if *b == b'\n' { b'\n' } else { b' ' })
        .collect();
    let prefix_str = std::str::from_utf8(&padded_prefix).map_err(|_| {
        errors::js_parse_error(
            Some((params_start as u32, params_end as u32)),
            "non-utf8",
        )
    })?;
    let synthetic = format!("{prefix_str}{params_slice} => {{}}");

    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts);
    let parser = OxcParser::new(&allocator, &synthetic, source_type)
        .with_options(svelte_compatible_options());
    let expression = match parser.parse_expression() {
        Ok(e) => e,
        Err(diags) => {
            let msg = diags
                .iter()
                .map(|d| format!("{d}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(errors::js_parse_error(
                Some((params_start as u32, params_end as u32)),
                &msg,
            ));
        }
    };

    let mut s = PrettyTSSerializer::new(false);
    expression.serialize(&mut s);
    let raw_json = s.into_string();
    let mut value: Value = serde_json::from_str(&raw_json).map_err(|e| {
        errors::js_parse_error(
            Some((params_start as u32, params_end as u32)),
            &e.to_string(),
        )
    })?;

    let synthetic_line_map = LineMap::new(&synthetic);
    normalize_template_elements(&mut value);
    shift_and_locate(&mut value, 0, &synthetic_line_map);
    unwrap_parens(&mut value);
    strip_empty_ts_defaults(&mut value);

    let Some(map) = value.as_object_mut() else {
        return Err(errors::js_parse_error(
            Some((params_start as u32, params_end as u32)),
            "expected ArrowFunctionExpression at root",
        ));
    };
    let params = map.remove("params").unwrap_or(Value::Array(vec![]));
    match params {
        Value::Array(arr) => Ok(arr),
        _ => Err(errors::js_parse_error(
            Some((params_start as u32, params_end as u32)),
            "ArrowFunctionExpression.params was not an array",
        )),
    }
}

struct ProtoComment {
    line: bool,
    start: u32,
    end: u32,
}

fn collect_comments_for_slice(allocator: &Allocator, slice: &str, ts: bool) -> Vec<ProtoComment> {
    let source_type = SourceType::default().with_typescript(ts);
    let parser = OxcParser::new(allocator, slice, source_type)
        .with_options(svelte_compatible_options());
    let ret = parser.parse();
    ret.program
        .comments
        .iter()
        .map(|c| ProtoComment {
            line: matches!(c.kind, CommentKind::Line),
            start: c.span.start,
            end: c.span.end,
        })
        .collect()
}

/// Attach each comment as `leadingComments` on the deepest expression node
/// whose `start >= comment.end` (smallest such start). Useful for inside-
/// expression comments like `{(/**/ 42)}`.
fn attach_leading_comments_to_expression(node: &mut Value, comments: &[RawComment]) {
    for comment in comments {
        let mut best: Option<u32> = None;
        walk_starts(node, &mut |start| {
            if start >= comment.end && best.map_or(true, |b| start < b) {
                best = Some(start);
            }
        });
        if let Some(target_start) = best {
            attach_at_start_expr(node, target_start, comment);
        }
    }
}

fn walk_starts<F: FnMut(u32)>(node: &Value, f: &mut F) {
    match node {
        Value::Object(map) => {
            if let Some(s) = map.get("start").and_then(|v| v.as_u64()) {
                f(s as u32);
            }
            for (_, v) in map.iter() {
                walk_starts(v, f);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter() {
                walk_starts(v, f);
            }
        }
        _ => {}
    }
}

fn attach_at_start_expr(node: &mut Value, target_start: u32, comment: &RawComment) -> bool {
    let Value::Object(map) = node else {
        return false;
    };
    for (_, child) in map.iter_mut() {
        if attach_at_start_expr(child, target_start, comment) {
            return true;
        }
    }
    let Some(start) = map.get("start").and_then(|v| v.as_u64()) else {
        return false;
    };
    if start as u32 != target_start {
        return false;
    }
    let entry = serde_json::json!({
        "type": if comment.line { "Line" } else { "Block" },
        "value": comment.value,
        "start": comment.start,
        "end": comment.end,
    });
    let leading = map
        .entry("leadingComments".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(arr) = leading {
        arr.push(entry);
    }
    true
}

/// Recursively walk a serialized AST, adding `parent_offset` to `start`/`end`
/// fields and adding `loc: { start, end }` with line+column via the line map.
///
/// Acorn's `loc.{start,end}` shape is `{ line: number, column: number }` only
/// — no `character`. We match that.
fn shift_and_locate(node: &mut Value, parent_offset: usize, line_map: &LineMap) {
    match node {
        Value::Object(map) => {
            // Shift start/end first so the loc lookup uses absolute offsets.
            let mut start_abs: Option<usize> = None;
            let mut end_abs: Option<usize> = None;

            if let Some(s) = map.get("start").and_then(|v| v.as_u64()) {
                let abs = s as usize + parent_offset;
                start_abs = Some(abs);
                map.insert("start".to_string(), Value::from(abs));
            }
            if let Some(e) = map.get("end").and_then(|v| v.as_u64()) {
                let abs = e as usize + parent_offset;
                end_abs = Some(abs);
                map.insert("end".to_string(), Value::from(abs));
            }

            // Inject `loc` for nodes that have positions. Skip if already there.
            if let (Some(s), Some(e)) = (start_abs, end_abs) {
                if !map.contains_key("loc") {
                    let (s_line, s_col) = line_map.locate(s);
                    let (e_line, e_col) = line_map.locate(e);
                    map.insert(
                        "loc".to_string(),
                        serde_json::json!({
                            "start": { "line": s_line, "column": s_col },
                            "end":   { "line": e_line, "column": e_col }
                        }),
                    );
                }
            }

            // Recurse into all children.
            for (_, v) in map.iter_mut() {
                shift_and_locate(v, parent_offset, line_map);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                shift_and_locate(v, parent_offset, line_map);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_literal() {
        let src = "{42}";
        let line_map = LineMap::new(src);
        let v = parse_expression(src, &line_map, 1, 3, false).unwrap();
        assert_eq!(v["type"], "Literal");
        assert_eq!(v["value"], 42);
        assert_eq!(v["start"], 1);
        assert_eq!(v["end"], 3);
        assert_eq!(v["loc"]["start"]["line"], 1);
        assert_eq!(v["loc"]["start"]["column"], 1);
    }

    #[test]
    fn parses_identifier() {
        let src = "{foo}";
        let line_map = LineMap::new(src);
        let v = parse_expression(src, &line_map, 1, 4, false).unwrap();
        assert_eq!(v["type"], "Identifier");
        assert_eq!(v["name"], "foo");
        assert_eq!(v["start"], 1);
        assert_eq!(v["end"], 4);
    }

    #[test]
    fn shifts_positions_in_nested_expression() {
        // Slice is at offset 5 of the original.
        let src = "abc  foo.bar";
        let line_map = LineMap::new(src);
        let v = parse_expression(src, &line_map, 5, 12, false).unwrap();
        assert_eq!(v["type"], "MemberExpression");
        assert_eq!(v["start"], 5);
        assert_eq!(v["end"], 12);
        // The `object` and `property` sub-trees also need shifted offsets.
        assert_eq!(v["object"]["type"], "Identifier");
        assert_eq!(v["object"]["start"], 5);
        assert_eq!(v["object"]["end"], 8);
        assert_eq!(v["property"]["start"], 9);
        assert_eq!(v["property"]["end"], 12);
    }

    #[test]
    fn ts_type_annotations() {
        // `(x: number) => x` is TS-only — should parse with ts=true.
        let src = "(x: number) => x";
        let line_map = LineMap::new(src);
        let v = parse_expression(src, &line_map, 0, src.len(), true).unwrap();
        assert_eq!(v["type"], "ArrowFunctionExpression");
    }

    #[test]
    fn rejects_invalid_js() {
        let src = "{!}";
        let line_map = LineMap::new(src);
        let r = parse_expression(src, &line_map, 1, 2, false);
        assert!(r.is_err());
    }

    #[test]
    fn parse_at_returns_end_offset() {
        // Source containing `{foo + bar}` at offset 5. We start parsing at
        // offset 6 (after `{`) and expect OXC to consume `foo + bar` and
        // return abs_end = position just before `}`.
        let src = "abcde{foo + bar}xyz";
        let line_map = LineMap::new(src);
        let (v, end) = parse_expression_at(src, &line_map, 6, false).unwrap();
        assert_eq!(v["type"], "BinaryExpression");
        // `foo + bar` is 9 characters → end = 6 + 9 = 15
        assert_eq!(end, 15);
        // The `}` at position 15 wasn't consumed.
        assert_eq!(&src[end..end + 1], "}");
    }

    #[test]
    fn parse_at_handles_string_literal_with_braces() {
        // `{"}"}` — the `}` inside the string must not terminate the expression.
        let src = r#"{"}"}"#;
        let line_map = LineMap::new(src);
        let (v, end) = parse_expression_at(src, &line_map, 1, false).unwrap();
        assert_eq!(v["type"], "Literal");
        assert_eq!(v["value"], "}");
        // String spans `"}"` — 3 chars starting at 1 → end = 4
        assert_eq!(end, 4);
    }
}
