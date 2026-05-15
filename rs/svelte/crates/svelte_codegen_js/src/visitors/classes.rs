//! Class declaration / expression visitors.

use serde_json::Value;

use crate::context::Context;

pub fn class_declaration(node: &Value, ctx: &mut Context) {
    class_common(node, ctx, true);
}

pub fn class_expression(node: &Value, ctx: &mut Context) {
    class_common(node, ctx, false);
}

fn class_common(node: &Value, ctx: &mut Context, decl: bool) {
    ctx.write("class", Some(node));
    if let Some(id) = node.get("id") {
        if !id.is_null() {
            ctx.write(" ", None);
            ctx.visit(id);
        } else if decl {
            // anonymous declaration — unusual; emit nothing extra
        }
    }
    if let Some(sc) = node.get("superClass") {
        if !sc.is_null() {
            ctx.write(" extends ", None);
            ctx.visit(sc);
        }
    }
    ctx.write(" ", None);
    ctx.visit(&node["body"]);
}

pub fn class_body(node: &Value, ctx: &mut Context) {
    ctx.write("{", Some(node));
    let body = node
        .get("body")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if body.is_empty() {
        ctx.write("}", None);
        return;
    }
    ctx.indent();
    ctx.newline();
    crate::visitors::programs::emit_body(node, ctx);
    ctx.dedent();
    ctx.newline();
    ctx.write("}", None);
}

pub fn method_definition(node: &Value, ctx: &mut Context) {
    let is_static = node
        .get("static")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let kind = node
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("method");
    let computed = node
        .get("computed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let val = &node["value"];
    let is_async = val.get("async").and_then(|v| v.as_bool()).unwrap_or(false);
    let is_generator = val
        .get("generator")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_static {
        ctx.write("static ", Some(node));
    }
    if kind == "get" || kind == "set" {
        ctx.write(kind, None);
        ctx.write(" ", None);
    }
    if is_async {
        ctx.write("async ", None);
    }
    if is_generator {
        ctx.write("*", None);
    }
    if computed {
        ctx.write("[", None);
        ctx.visit(&node["key"]);
        ctx.write("]", None);
    } else {
        ctx.visit(&node["key"]);
    }
    let params = val
        .get("params")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    ctx.write("(", None);
    crate::visitors::programs::sequence(ctx, &params, false);
    ctx.write(") ", None);
    ctx.visit(&val["body"]);
}

pub fn property_definition(node: &Value, ctx: &mut Context) {
    let is_static = node
        .get("static")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let computed = node
        .get("computed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_static {
        ctx.write("static ", Some(node));
    }
    if computed {
        ctx.write("[", None);
        ctx.visit(&node["key"]);
        ctx.write("]", None);
    } else {
        ctx.visit(&node["key"]);
    }
    if let Some(val) = node.get("value") {
        if !val.is_null() {
            ctx.write(" = ", None);
            ctx.visit(val);
        }
    }
    ctx.write(";", None);
}

pub fn static_block(node: &Value, ctx: &mut Context) {
    ctx.write("static {", Some(node));
    let body = node
        .get("body")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if body.is_empty() {
        ctx.write("}", None);
        return;
    }
    ctx.indent();
    ctx.newline();
    crate::visitors::programs::emit_body(node, ctx);
    ctx.dedent();
    ctx.newline();
    ctx.write("}", None);
}
