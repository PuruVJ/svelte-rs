//! Declaration visitors: VariableDeclaration, FunctionDeclaration.

use serde_json::Value;

use crate::context::Context;

pub fn variable_declaration(node: &Value, ctx: &mut Context) {
    use crate::comments::flush_comments_until;
    let kind = node.get("kind").and_then(|v| v.as_str()).unwrap_or("var");
    ctx.write(kind, Some(node));
    ctx.write(" ", None);
    let declarations = node
        .get("declarations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Detect any comment whose position falls between two declarators (i.e.
    // before declarator[N].loc.start but after declarator[N-1].loc.end). When
    // present, switch to a multi-line layout with comments emitted on their
    // own indented lines.
    fn loc_start(d: &Value) -> Option<(u32, u32)> {
        // Prefer declarator's own loc; fall back to its id's loc when missing.
        let p = d
            .get("loc")
            .and_then(|l| l.get("start"))
            .or_else(|| d.get("id")?.get("loc")?.get("start"))?;
        Some((
            p.get("line")?.as_u64()? as u32,
            p.get("column")?.as_u64()? as u32,
        ))
    }
    fn loc_end(d: &Value) -> Option<(u32, u32)> {
        let p = d
            .get("loc")
            .and_then(|l| l.get("end"))
            .or_else(|| d.get("id")?.get("loc")?.get("end"))?;
        Some((
            p.get("line")?.as_u64()? as u32,
            p.get("column")?.as_u64()? as u32,
        ))
    }

    let has_inter_comment = declarations.len() > 1 && {
        let state = ctx.comment_state();
        let s = state.borrow();
        let comments = &s.comments;
        let mut found = false;
        for i in 1..declarations.len() {
            let after = loc_end(&declarations[i - 1]);
            let before = loc_start(&declarations[i]);
            if let (Some(after), Some(before)) = (after, before) {
                for c in comments {
                    let cs = c.get("loc").and_then(|l| l.get("start")).and_then(|p| {
                        Some((
                            p.get("line")?.as_u64()? as u32,
                            p.get("column")?.as_u64()? as u32,
                        ))
                    });
                    if let Some(cs) = cs {
                        if crate::comments::before(after, cs)
                            && crate::comments::before(cs, before)
                        {
                            found = true;
                            break;
                        }
                    }
                }
                if found {
                    break;
                }
            }
        }
        found
    };

    if has_inter_comment {
        ctx.indent();
        for (i, d) in declarations.iter().enumerate() {
            if i > 0 {
                ctx.write(",", None);
                ctx.newline();
            }
            if let Some(start) = loc_start(d) {
                flush_comments_until(ctx, None, Some(start), false);
            }
            ctx.visit(d);
        }
        ctx.dedent();
    } else {
        for (i, d) in declarations.iter().enumerate() {
            if i > 0 {
                ctx.write(", ", None);
            }
            ctx.visit(d);
        }
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
