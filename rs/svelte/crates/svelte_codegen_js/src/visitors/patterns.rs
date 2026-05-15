//! Destructuring pattern visitors.

use serde_json::Value;

use crate::context::Context;
use crate::visitors::helpers::type_of;

pub fn array_pattern(node: &Value, ctx: &mut Context) {
    ctx.write("[", Some(node));
    let elements = node
        .get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    crate::visitors::programs::sequence(ctx, &elements, false);
    ctx.write("]", None);
}

pub fn object_pattern(node: &Value, ctx: &mut Context) {
    ctx.write("{", Some(node));
    let properties = node
        .get("properties")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    crate::visitors::programs::sequence(ctx, &properties, true);
    ctx.write("}", None);
}

pub fn rest_element(node: &Value, ctx: &mut Context) {
    ctx.write("...", Some(node));
    ctx.visit(&node["argument"]);
}

pub fn assignment_pattern(node: &Value, ctx: &mut Context) {
    ctx.visit(&node["left"]);
    ctx.write(" = ", None);
    ctx.visit(&node["right"]);
}

/// Properties inside object patterns use the same `Property` shape but with
/// `shorthand`/`computed` flags; reuse the `property` visitor by acting as a
/// thin adapter. Note: many ASTs emit `Property` for both expression and pattern
/// contexts — that's why it's already wired to the same fn.
pub fn pattern_property(node: &Value, ctx: &mut Context) {
    let computed = node
        .get("computed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let shorthand = node
        .get("shorthand")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if computed {
        ctx.write("[", None);
        ctx.visit(&node["key"]);
        ctx.write("]", None);
    } else {
        ctx.visit(&node["key"]);
    }
    if !shorthand || type_of(&node["value"]) == "AssignmentPattern" {
        if !shorthand {
            ctx.write(": ", None);
        }
        if shorthand && type_of(&node["value"]) == "AssignmentPattern" {
            // shorthand-with-default: only emit `= default`
            ctx.write(" = ", None);
            ctx.visit(&node["value"]["right"]);
        } else {
            ctx.visit(&node["value"]);
        }
    }
}
