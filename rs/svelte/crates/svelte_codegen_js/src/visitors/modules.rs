//! Module-level visitors: imports and exports.

use serde_json::Value;

use crate::context::Context;
use crate::visitors::helpers::type_of;

pub fn import_declaration(node: &Value, ctx: &mut Context) {
    ctx.write("import ", Some(node));
    let specifiers = node
        .get("specifiers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if specifiers.is_empty() {
        ctx.visit(&node["source"]);
        ctx.write(";", None);
        return;
    }

    let mut default_spec: Option<&Value> = None;
    let mut namespace_spec: Option<&Value> = None;
    let mut named_specs: Vec<&Value> = Vec::new();
    for s in &specifiers {
        match type_of(s) {
            "ImportDefaultSpecifier" => default_spec = Some(s),
            "ImportNamespaceSpecifier" => namespace_spec = Some(s),
            _ => named_specs.push(s),
        }
    }

    let mut first = true;
    if let Some(d) = default_spec {
        ctx.visit(&d["local"]);
        first = false;
    }
    if let Some(n) = namespace_spec {
        if !first {
            ctx.write(", ", None);
        }
        ctx.write("* as ", None);
        ctx.visit(&n["local"]);
        first = false;
    }
    if !named_specs.is_empty() {
        if !first {
            ctx.write(", ", None);
        }
        ctx.write("{", None);
        // emit each specifier as a small ad-hoc value so sequence() can layout them
        let nodes: Vec<Value> = named_specs
            .iter()
            .map(|n| serde_json::json!({
                "type": "ImportSpecifier",
                "imported": n["imported"].clone(),
                "local": n["local"].clone()
            }))
            .collect();
        crate::visitors::programs::sequence(ctx, &nodes, true);
        ctx.write("}", None);
    }
    ctx.write(" from ", None);
    ctx.visit(&node["source"]);
    ctx.write(";", None);
}

pub fn export_named_declaration(node: &Value, ctx: &mut Context) {
    ctx.write("export ", Some(node));
    if let Some(decl) = node.get("declaration") {
        if !decl.is_null() {
            ctx.visit(decl);
            return;
        }
    }
    ctx.write("{ ", None);
    let specifiers = node
        .get("specifiers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, s) in specifiers.iter().enumerate() {
        if i > 0 {
            ctx.write(", ", None);
        }
        let local = &s["local"];
        let exported = &s["exported"];
        let ln = local.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let en = exported.get("name").and_then(|v| v.as_str()).unwrap_or("");
        ctx.visit(local);
        if ln != en {
            ctx.write(" as ", None);
            ctx.visit(exported);
        }
    }
    ctx.write(" }", None);
    if let Some(src) = node.get("source") {
        if !src.is_null() {
            ctx.write(" from ", None);
            ctx.visit(src);
        }
    }
    ctx.write(";", None);
}

pub fn export_default_declaration(node: &Value, ctx: &mut Context) {
    ctx.write("export default ", Some(node));
    ctx.visit(&node["declaration"]);
    let ty = type_of(&node["declaration"]);
    if !matches!(
        ty,
        "FunctionDeclaration" | "ClassDeclaration" | "FunctionExpression" | "ClassExpression"
    ) {
        ctx.write(";", None);
    }
}

pub fn export_all_declaration(node: &Value, ctx: &mut Context) {
    ctx.write("export * ", Some(node));
    if let Some(exp) = node.get("exported") {
        if !exp.is_null() {
            ctx.write("as ", None);
            ctx.visit(exp);
            ctx.write(" ", None);
        }
    }
    ctx.write("from ", None);
    ctx.visit(&node["source"]);
    ctx.write(";", None);
}

pub fn import_specifier(node: &Value, ctx: &mut Context) {
    // Standalone usage of an ImportSpecifier is unusual — the outer
    // ImportDeclaration visitor renders these inline. Provide a sensible
    // fallback emit for completeness.
    let imp = &node["imported"];
    let local = &node["local"];
    let in_name = imp.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let ln_name = local.get("name").and_then(|v| v.as_str()).unwrap_or("");
    ctx.visit(imp);
    if in_name != ln_name {
        ctx.write(" as ", None);
        ctx.visit(local);
    }
}
