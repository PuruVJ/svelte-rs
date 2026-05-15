//! Shared helpers used across visitor implementations.
//!
//! Ported from `esrap/src/languages/ts/index.js` — the closure-captured
//! helpers (`write_comment`, `sequence`, `EXPRESSIONS_PRECEDENCE`, ...).

use serde_json::Value;

use crate::context::Context;

/// Mirror of `EXPRESSIONS_PRECEDENCE` in `ts/index.js:8-46`.
pub fn expression_precedence(ty: &str) -> i32 {
    match ty {
        "JSXFragment" | "JSXElement" | "ArrayPattern" | "ObjectPattern" | "ArrayExpression"
        | "TaggedTemplateExpression" | "ThisExpression" | "Identifier" | "TemplateLiteral"
        | "Super" | "SequenceExpression" => 20,
        "MemberExpression" | "MetaProperty" | "CallExpression" | "ChainExpression"
        | "ImportExpression" | "NewExpression" => 19,
        "Literal" | "TSSatisfiesExpression" | "TSInstantiationExpression" | "TSNonNullExpression"
        | "TSTypeAssertion" => 18,
        "AwaitExpression" | "ClassExpression" | "FunctionExpression" | "ObjectExpression" => 17,
        "TSAsExpression" | "UpdateExpression" => 16,
        "UnaryExpression" => 15,
        "BinaryExpression" => 14,
        "LogicalExpression" => 13,
        "ConditionalExpression" => 4,
        "ArrowFunctionExpression" | "AssignmentExpression" => 3,
        "YieldExpression" => 2,
        "RestElement" => 1,
        _ => 0,
    }
}

/// Mirror of `OPERATOR_PRECEDENCE` in `ts/index.js:48-74`.
pub fn operator_precedence(op: &str) -> i32 {
    match op {
        "||" => 2,
        "&&" => 3,
        "??" => 4,
        "|" => 5,
        "^" => 6,
        "&" => 7,
        "==" | "!=" | "===" | "!==" => 8,
        "<" | ">" | "<=" | ">=" | "in" | "instanceof" => 9,
        "<<" | ">>" | ">>>" => 10,
        "+" | "-" => 11,
        "*" | "%" | "/" => 12,
        "**" => 13,
        _ => 0,
    }
}

/// Return the type tag of a JSON node, or `""` if missing.
pub fn type_of(node: &Value) -> &str {
    node.get("type").and_then(|v| v.as_str()).unwrap_or("")
}

/// `true` when `comment.value` contains no `\n`. Tracks `comment.type === 'Block' && !value.includes('\n')`.
pub fn is_single_line_block(comment: &Value) -> bool {
    matches!(comment.get("type").and_then(|v| v.as_str()), Some("Block"))
        && !comment
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains('\n')
}

/// Port of `write_comment` — see `ts/index.js:80-95`.
pub fn write_comment(comment: &Value, ctx: &mut Context) {
    let kind = comment.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let value = comment.get("value").and_then(|v| v.as_str()).unwrap_or("");
    if kind == "Line" {
        ctx.write(&format!("//{value}"), None);
    } else {
        ctx.write("/*", None);
        let lines: Vec<&str> = value.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                ctx.newline();
            }
            ctx.write(line, None);
        }
        ctx.write("*/", None);
        if lines.len() > 1 {
            ctx.newline();
        }
    }
}

/// Get `node.loc.start` as `(line, column)`, if present.
pub fn loc_start(node: &Value) -> Option<(u32, u32)> {
    let loc = node.get("loc")?;
    let start = loc.get("start")?;
    Some((
        start.get("line")?.as_u64()? as u32,
        start.get("column")?.as_u64()? as u32,
    ))
}

/// Get `node.loc.end` as `(line, column)`, if present.
pub fn loc_end(node: &Value) -> Option<(u32, u32)> {
    let loc = node.get("loc")?;
    let end = loc.get("end")?;
    Some((
        end.get("line")?.as_u64()? as u32,
        end.get("column")?.as_u64()? as u32,
    ))
}
