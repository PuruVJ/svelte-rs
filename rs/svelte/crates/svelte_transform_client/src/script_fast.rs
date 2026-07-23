//! Fast-path script analysis for common `$props()`-only instance scripts.

use std::collections::HashSet;

use svelte_ast::root::Script;
use svelte_js_ast::*;

use crate::walker::ScriptInfo;

/// When the instance script is only `let { … } = $props();`, skip full rune/legacy scans.
pub fn analyze_script_props_only(
    instance: Option<&Script>,
    template_assigned: &HashSet<String>,
) -> Option<ScriptInfo> {
    let script = instance?;
    let body = &script.content.body;
    if body.len() != 1 {
        return None;
    }
    let Statement::Variable(v) = &body[0] else {
        return None;
    };
    if !matches!(v.kind, VariableKind::Const | VariableKind::Let) || v.declarations.len() != 1 {
        return None;
    }
    let decl = &v.declarations[0];
    let Pattern::Object(obj) = &decl.id else {
        return None;
    };
    let init = decl.init.as_ref()?;
    if !is_props_call_fast(init) {
        return None;
    }
    let mut props_destructured = HashSet::new();
    for m in &obj.properties {
        if let ObjectPatternMember::Property(p) = m {
            if let PropertyKey::Identifier(id) = &p.key {
                props_destructured.insert(id.name.to_string());
            }
        } else {
            return None;
        }
    }
    let _ = template_assigned;
    Some(ScriptInfo::props_only(props_destructured))
}

fn is_props_call_fast(e: &Expression) -> bool {
    match e {
        Expression::Call(c) => matches!(
            &c.callee,
            Expression::Identifier(i) if i.name == "$props"
        ),
        _ => false,
    }
}
