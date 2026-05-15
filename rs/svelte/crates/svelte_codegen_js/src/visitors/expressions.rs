//! Expression visitors. Ported from `esrap/src/languages/ts/index.js`.

use serde_json::Value;

use crate::context::Context;
use crate::visitors::helpers::{expression_precedence, operator_precedence, type_of};

/// Determine whether `child` (an expression appearing in `position` of `parent`)
/// requires parentheses. Mirrors esrap's precedence-based wrap decision (the
/// inline `needs_parens` checks throughout `ts/index.js`).
///
/// `position` is one of `"left"`, `"right"`, `"argument"`, `"callee"`, etc.
fn needs_parens(parent: &Value, child: &Value, position: &str) -> bool {
    let pt = type_of(parent);
    let ct = type_of(child);
    let pp = expression_precedence(pt);
    let cp = expression_precedence(ct);
    if cp > pp {
        return false;
    }
    if cp < pp {
        return true;
    }
    if ct == "BinaryExpression" || ct == "LogicalExpression" {
        if pt == "BinaryExpression" || pt == "LogicalExpression" {
            let po = parent
                .get("operator")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let co = child.get("operator").and_then(|v| v.as_str()).unwrap_or("");
            let pop = operator_precedence(po);
            let cop = operator_precedence(co);
            if cop > pop {
                return false;
            }
            if cop < pop {
                return true;
            }
            // same precedence — right side needs parens for `**`, otherwise left
            if position == "right" {
                return po != "**";
            }
            return po == "**";
        }
    }
    false
}

/// Emit `child`, wrapping in parens when precedence requires it.
pub(crate) fn visit_wrapped(parent: &Value, child: &Value, position: &str, ctx: &mut Context) {
    if needs_parens(parent, child, position) {
        ctx.write("(", None);
        ctx.visit(child);
        ctx.write(")", None);
    } else {
        ctx.visit(child);
    }
}

pub fn binary_expression(node: &Value, ctx: &mut Context) {
    let op = node.get("operator").and_then(|v| v.as_str()).unwrap_or("");
    visit_wrapped(node, &node["left"], "left", ctx);
    ctx.write(&format!(" {op} "), None);
    visit_wrapped(node, &node["right"], "right", ctx);
}

pub fn logical_expression(node: &Value, ctx: &mut Context) {
    binary_expression(node, ctx)
}

pub fn unary_expression(node: &Value, ctx: &mut Context) {
    let op = node.get("operator").and_then(|v| v.as_str()).unwrap_or("");
    let prefix = node
        .get("prefix")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if prefix {
        ctx.write(op, Some(node));
        // word operators require a space
        if matches!(op, "typeof" | "void" | "delete") {
            ctx.write(" ", None);
        }
        visit_wrapped(node, &node["argument"], "argument", ctx);
    } else {
        visit_wrapped(node, &node["argument"], "argument", ctx);
        ctx.write(op, None);
    }
}

pub fn update_expression(node: &Value, ctx: &mut Context) {
    let op = node.get("operator").and_then(|v| v.as_str()).unwrap_or("");
    let prefix = node
        .get("prefix")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if prefix {
        ctx.write(op, Some(node));
        visit_wrapped(node, &node["argument"], "argument", ctx);
    } else {
        visit_wrapped(node, &node["argument"], "argument", ctx);
        ctx.write(op, None);
    }
}

pub fn assignment_expression(node: &Value, ctx: &mut Context) {
    let op = node.get("operator").and_then(|v| v.as_str()).unwrap_or("=");
    ctx.visit(&node["left"]);
    ctx.write(&format!(" {op} "), None);
    visit_wrapped(node, &node["right"], "right", ctx);
}

pub fn conditional_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["test"], "test", ctx);
    ctx.write(" ? ", None);
    visit_wrapped(node, &node["consequent"], "consequent", ctx);
    ctx.write(" : ", None);
    visit_wrapped(node, &node["alternate"], "alternate", ctx);
}

pub fn sequence_expression(node: &Value, ctx: &mut Context) {
    let exprs = node
        .get("expressions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    ctx.write("(", Some(node));
    crate::visitors::programs::sequence(ctx, &exprs, false);
    ctx.write(")", None);
}

pub fn member_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["object"], "object", ctx);
    let computed = node
        .get("computed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let optional = node
        .get("optional")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if computed {
        ctx.write(if optional { "?.[" } else { "[" }, None);
        ctx.visit(&node["property"]);
        ctx.write("]", None);
    } else {
        ctx.write(if optional { "?." } else { "." }, None);
        ctx.visit(&node["property"]);
    }
}

pub fn chain_expression(node: &Value, ctx: &mut Context) {
    ctx.visit(&node["expression"]);
}

pub fn call_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["callee"], "callee", ctx);
    let optional = node
        .get("optional")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if optional {
        ctx.write("?.", None);
    }
    emit_call_arguments(node, ctx);
}

pub fn new_expression(node: &Value, ctx: &mut Context) {
    ctx.write("new ", Some(node));
    visit_wrapped(node, &node["callee"], "callee", ctx);
    emit_call_arguments(node, ctx);
}

/// Port of upstream's `CallExpression|NewExpression` argument-emitting logic
/// (`ts/index.js:482-540`). Crucial detail: the multi-line decision is based on
/// the *non-last* arguments only — the last argument can be arbitrarily long
/// (e.g. a giant template literal) without forcing the whole call to wrap.
fn emit_call_arguments(node: &Value, ctx: &mut Context) {
    let args = node
        .get("arguments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    ctx.write("(", None);

    if args.is_empty() {
        ctx.write(")", None);
        return;
    }

    // Build separate contexts for non-last and last args so their multiline
    // flags can be considered independently.
    let mut child_context = ctx.fresh(); // all non-last args
    let mut final_context = ctx.fresh(); // last arg

    for (i, arg) in args.iter().enumerate() {
        let is_last = i == args.len() - 1;
        let target = if is_last {
            &mut final_context
        } else {
            &mut child_context
        };
        target.visit(arg);
        if !is_last {
            target.write(",", None);
            target.write(" ", None);
        }
    }

    let multiline = child_context.is_multiline();

    if multiline {
        ctx.indent();
        ctx.newline();
    }
    ctx.append(child_context);
    ctx.append(final_context);
    if multiline {
        ctx.dedent();
        ctx.newline();
    }
    ctx.write(")", None);
}

pub fn array_expression(node: &Value, ctx: &mut Context) {
    ctx.write("[", Some(node));
    let elements = node
        .get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    crate::visitors::programs::sequence(ctx, &elements, false);
    ctx.write("]", None);
}

pub fn object_expression(node: &Value, ctx: &mut Context) {
    ctx.write("{", Some(node));
    let properties = node
        .get("properties")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    crate::visitors::programs::sequence(ctx, &properties, true);
    ctx.write("}", None);
}

pub fn property(node: &Value, ctx: &mut Context) {
    let kind = node.get("kind").and_then(|v| v.as_str()).unwrap_or("init");
    let shorthand = node
        .get("shorthand")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let computed = node
        .get("computed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let method = node
        .get("method")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if kind == "get" || kind == "set" {
        ctx.write(kind, None);
        ctx.write(" ", None);
    }
    if computed {
        ctx.write("[", None);
        ctx.visit(&node["key"]);
        ctx.write("]", None);
    } else {
        ctx.visit(&node["key"]);
    }
    if method || kind == "get" || kind == "set" {
        let val = &node["value"];
        let params = val
            .get("params")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        ctx.write("(", None);
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                ctx.write(", ", None);
            }
            ctx.visit(p);
        }
        ctx.write(") ", None);
        ctx.visit(&val["body"]);
    } else if !shorthand {
        ctx.write(": ", None);
        ctx.visit(&node["value"]);
    }
}

pub fn spread_element(node: &Value, ctx: &mut Context) {
    ctx.write("...", Some(node));
    ctx.visit(&node["argument"]);
}

pub fn arrow_function_expression(node: &Value, ctx: &mut Context) {
    let is_async = node
        .get("async")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_async {
        ctx.write("async ", Some(node));
    }
    let params = node
        .get("params")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if params.len() == 1
        && type_of(&params[0]) == "Identifier"
        && params[0].get("typeAnnotation").is_none()
    {
        ctx.visit(&params[0]);
    } else {
        ctx.write("(", None);
        crate::visitors::programs::sequence(ctx, &params, false);
        ctx.write(")", None);
    }
    ctx.write(" => ", None);
    let body = &node["body"];
    if type_of(body) == "BlockStatement" {
        ctx.visit(body);
    } else {
        // wrap in parens if body is an ObjectExpression to avoid ambiguity
        if type_of(body) == "ObjectExpression" {
            ctx.write("(", None);
            ctx.visit(body);
            ctx.write(")", None);
        } else {
            ctx.visit(body);
        }
    }
}

pub fn function_expression(node: &Value, ctx: &mut Context) {
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

pub fn await_expression(node: &Value, ctx: &mut Context) {
    ctx.write("await ", Some(node));
    visit_wrapped(node, &node["argument"], "argument", ctx);
}

pub fn yield_expression(node: &Value, ctx: &mut Context) {
    let delegate = node
        .get("delegate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    ctx.write(if delegate { "yield*" } else { "yield" }, Some(node));
    if let Some(arg) = node.get("argument") {
        if !arg.is_null() {
            ctx.write(" ", None);
            visit_wrapped(node, arg, "argument", ctx);
        }
    }
}

pub fn import_expression(node: &Value, ctx: &mut Context) {
    ctx.write("import(", Some(node));
    ctx.visit(&node["source"]);
    if let Some(opts) = node.get("options") {
        if !opts.is_null() {
            ctx.write(", ", None);
            ctx.visit(opts);
        }
    }
    ctx.write(")", None);
}

pub fn tagged_template_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["tag"], "tag", ctx);
    ctx.visit(&node["quasi"]);
}
