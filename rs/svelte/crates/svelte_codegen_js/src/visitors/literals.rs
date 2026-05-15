//! Literal / TemplateLiteral / TemplateElement visitors.
//!
//! Ported from `esrap/src/languages/ts/index.js`.

use serde_json::Value;

use crate::context::Context;

/// `Literal`: emit `raw` if present, otherwise stringify `value`.
/// Acorn keeps the original source spelling in `raw` — preserve it for
/// byte-exact output.
pub fn literal(node: &Value, ctx: &mut Context) {
    if let Some(raw) = node.get("raw").and_then(|v| v.as_str()) {
        ctx.write(raw, Some(node));
        return;
    }
    let val = node.get("value");
    let s = match val {
        Some(Value::Null) | None => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::String(s)) => format!("\"{s}\""),
        Some(Value::Number(n)) => n.to_string(),
        Some(v) => v.to_string(),
    };
    ctx.write(&s, Some(node));
}

/// `TemplateLiteral`: zip `quasis` with `expressions`. See upstream visitor.
pub fn template_literal(node: &Value, ctx: &mut Context) {
    ctx.write("`", Some(node));
    let quasis = node
        .get("quasis")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let expressions = node
        .get("expressions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, q) in quasis.iter().enumerate() {
        let raw = q
            .get("value")
            .and_then(|v| v.get("raw"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        ctx.write(raw, None);
        if i < expressions.len() {
            ctx.write("${", None);
            ctx.visit(&expressions[i]);
            ctx.write("}", None);
        }
    }
    ctx.write("`", None);
}
