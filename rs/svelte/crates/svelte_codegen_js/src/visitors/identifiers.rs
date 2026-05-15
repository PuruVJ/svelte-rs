//! Identifier / ThisExpression / Super / PrivateIdentifier visitors.

use serde_json::Value;

use crate::context::Context;

pub fn identifier(node: &Value, ctx: &mut Context) {
    let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
    ctx.write(name, Some(node));
}

pub fn private_identifier(node: &Value, ctx: &mut Context) {
    let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
    ctx.write(&format!("#{name}"), Some(node));
}

pub fn this_expression(node: &Value, ctx: &mut Context) {
    ctx.write("this", Some(node));
}

pub fn super_expression(node: &Value, ctx: &mut Context) {
    ctx.write("super", Some(node));
}
