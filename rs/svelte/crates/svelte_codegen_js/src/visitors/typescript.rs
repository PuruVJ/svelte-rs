//! TypeScript-specific visitors. Most TS surface is stripped by the OXC bridge
//! before codegen (we transpile to plain JS). These visitors handle the
//! remaining TS constructs that survive into output (`TSAsExpression`, etc.).

use serde_json::Value;

use crate::context::Context;
use crate::visitors::expressions::visit_wrapped;

pub fn ts_as_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["expression"], "expression", ctx);
    // Type annotation is stripped — most Svelte outputs are pure JS, so emit
    // just the expression. If a TS-flavored output is ever needed, port the
    // `TSAsExpression` visitor from `ts/index.js`.
}

pub fn ts_satisfies_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["expression"], "expression", ctx);
}

pub fn ts_non_null_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["expression"], "expression", ctx);
}

pub fn ts_type_assertion(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["expression"], "expression", ctx);
}

pub fn ts_instantiation_expression(node: &Value, ctx: &mut Context) {
    visit_wrapped(node, &node["expression"], "expression", ctx);
}
