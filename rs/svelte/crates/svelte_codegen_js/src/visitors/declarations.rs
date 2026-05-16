//! Declaration visitors: VariableDeclaration, FunctionDeclaration.

use serde_json::Value;

use crate::context::Context;

pub fn variable_declaration(node: &Value, ctx: &mut Context) {
    let kind = node.get("kind").and_then(|v| v.as_str()).unwrap_or("var");
    ctx.write(kind, Some(node));
    ctx.write(" ", None);
    let declarations = node
        .get("declarations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // Don't use sequence's length-based multi-line wrap — VariableDeclaration
    // upstream just comma-separates declarators on a single line regardless
    // of total length. (sequence() would force-break a single long declarator.)
    for (i, d) in declarations.iter().enumerate() {
        if i > 0 {
            ctx.write(", ", None);
        }
        ctx.visit(d);
    }
    ctx.write(";", None);
}

pub fn variable_declarator(node: &Value, ctx: &mut Context) {
    ctx.visit(&node["id"]);
    if let Some(init) = node.get("init") {
        if !init.is_null() {
            ctx.write(" = ", None);
            ctx.visit(init);
        }
    }
}

pub fn function_declaration(node: &Value, ctx: &mut Context) {
    let is_async = node
        .get("async")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let is_generator = node
        .get("generator")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_async {
        ctx.write("async ", Some(node));
    }
    ctx.write("function", Some(node));
    if is_generator {
        ctx.write("*", None);
    }
    if let Some(id) = node.get("id") {
        if !id.is_null() {
            ctx.write(" ", None);
            ctx.visit(id);
        }
    }
    let params = node
        .get("params")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    ctx.write("(", None);
    crate::visitors::programs::sequence(ctx, &params, false);
    ctx.write(") ", None);
    ctx.visit(&node["body"]);
}
