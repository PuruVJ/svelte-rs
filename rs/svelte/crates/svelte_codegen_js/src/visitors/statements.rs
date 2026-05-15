//! Statement visitors.

use serde_json::Value;

use crate::context::Context;
use crate::visitors::helpers::type_of;

pub fn expression_statement(node: &Value, ctx: &mut Context) {
    let expr = &node["expression"];
    // wrap when expression starts with `function`/`class`/`{` to avoid parse ambiguity
    let needs_wrap = matches!(
        type_of(expr),
        "FunctionExpression" | "ClassExpression" | "ObjectExpression"
    );
    if needs_wrap {
        ctx.write("(", Some(node));
        ctx.visit(expr);
        ctx.write(")", None);
    } else {
        ctx.visit(expr);
    }
    ctx.write(";", None);
}

pub fn block_statement(node: &Value, ctx: &mut Context) {
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

pub fn empty_statement(node: &Value, ctx: &mut Context) {
    ctx.write(";", Some(node));
}

pub fn debugger_statement(node: &Value, ctx: &mut Context) {
    ctx.write("debugger;", Some(node));
}

pub fn return_statement(node: &Value, ctx: &mut Context) {
    ctx.write("return", Some(node));
    if let Some(arg) = node.get("argument") {
        if !arg.is_null() {
            ctx.write(" ", None);
            ctx.visit(arg);
        }
    }
    ctx.write(";", None);
}

pub fn break_statement(node: &Value, ctx: &mut Context) {
    ctx.write("break", Some(node));
    if let Some(label) = node.get("label") {
        if !label.is_null() {
            ctx.write(" ", None);
            ctx.visit(label);
        }
    }
    ctx.write(";", None);
}

pub fn continue_statement(node: &Value, ctx: &mut Context) {
    ctx.write("continue", Some(node));
    if let Some(label) = node.get("label") {
        if !label.is_null() {
            ctx.write(" ", None);
            ctx.visit(label);
        }
    }
    ctx.write(";", None);
}

pub fn throw_statement(node: &Value, ctx: &mut Context) {
    ctx.write("throw ", Some(node));
    ctx.visit(&node["argument"]);
    ctx.write(";", None);
}

pub fn if_statement(node: &Value, ctx: &mut Context) {
    ctx.write("if (", Some(node));
    ctx.visit(&node["test"]);
    ctx.write(") ", None);
    ctx.visit(&node["consequent"]);
    if let Some(alt) = node.get("alternate") {
        if !alt.is_null() {
            ctx.write(" else ", None);
            ctx.visit(alt);
        }
    }
}

pub fn while_statement(node: &Value, ctx: &mut Context) {
    ctx.write("while (", Some(node));
    ctx.visit(&node["test"]);
    ctx.write(") ", None);
    ctx.visit(&node["body"]);
}

pub fn do_while_statement(node: &Value, ctx: &mut Context) {
    ctx.write("do ", Some(node));
    ctx.visit(&node["body"]);
    ctx.write(" while (", None);
    ctx.visit(&node["test"]);
    ctx.write(");", None);
}

pub fn for_statement(node: &Value, ctx: &mut Context) {
    ctx.write("for (", Some(node));
    if let Some(init) = node.get("init") {
        if !init.is_null() {
            ctx.visit(init);
            if type_of(init) != "VariableDeclaration" {
                ctx.write(";", None);
            }
        } else {
            ctx.write(";", None);
        }
    } else {
        ctx.write(";", None);
    }
    ctx.write(" ", None);
    if let Some(test) = node.get("test") {
        if !test.is_null() {
            ctx.visit(test);
        }
    }
    ctx.write("; ", None);
    if let Some(upd) = node.get("update") {
        if !upd.is_null() {
            ctx.visit(upd);
        }
    }
    ctx.write(") ", None);
    ctx.visit(&node["body"]);
}

pub fn for_in_statement(node: &Value, ctx: &mut Context) {
    ctx.write("for (", Some(node));
    ctx.visit(&node["left"]);
    ctx.write(" in ", None);
    ctx.visit(&node["right"]);
    ctx.write(") ", None);
    ctx.visit(&node["body"]);
}

pub fn for_of_statement(node: &Value, ctx: &mut Context) {
    let is_await = node
        .get("await")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_await {
        ctx.write("for await (", Some(node));
    } else {
        ctx.write("for (", Some(node));
    }
    ctx.visit(&node["left"]);
    ctx.write(" of ", None);
    ctx.visit(&node["right"]);
    ctx.write(") ", None);
    ctx.visit(&node["body"]);
}

pub fn try_statement(node: &Value, ctx: &mut Context) {
    ctx.write("try ", Some(node));
    ctx.visit(&node["block"]);
    if let Some(handler) = node.get("handler") {
        if !handler.is_null() {
            ctx.visit(handler);
        }
    }
    if let Some(fin) = node.get("finalizer") {
        if !fin.is_null() {
            ctx.write(" finally ", None);
            ctx.visit(fin);
        }
    }
}

pub fn catch_clause(node: &Value, ctx: &mut Context) {
    ctx.write(" catch ", None);
    if let Some(param) = node.get("param") {
        if !param.is_null() {
            ctx.write("(", None);
            ctx.visit(param);
            ctx.write(") ", None);
        }
    }
    ctx.visit(&node["body"]);
}

pub fn switch_statement(node: &Value, ctx: &mut Context) {
    ctx.write("switch (", Some(node));
    ctx.visit(&node["discriminant"]);
    ctx.write(") {", None);
    ctx.indent();
    ctx.newline();
    let cases = node
        .get("cases")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, c) in cases.iter().enumerate() {
        if i > 0 {
            ctx.newline();
        }
        ctx.visit(c);
    }
    ctx.dedent();
    ctx.newline();
    ctx.write("}", None);
}

pub fn switch_case(node: &Value, ctx: &mut Context) {
    if let Some(test) = node.get("test") {
        if !test.is_null() {
            ctx.write("case ", Some(node));
            ctx.visit(test);
            ctx.write(":", None);
        } else {
            ctx.write("default:", Some(node));
        }
    } else {
        ctx.write("default:", Some(node));
    }
    let consequent = node
        .get("consequent")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !consequent.is_empty() {
        ctx.indent();
        for stmt in &consequent {
            ctx.newline();
            ctx.visit(stmt);
        }
        ctx.dedent();
    }
}

pub fn labeled_statement(node: &Value, ctx: &mut Context) {
    ctx.visit(&node["label"]);
    ctx.write(": ", None);
    ctx.visit(&node["body"]);
}

pub fn with_statement(node: &Value, ctx: &mut Context) {
    ctx.write("with (", Some(node));
    ctx.visit(&node["object"]);
    ctx.write(") ", None);
    ctx.visit(&node["body"]);
}
