//! Direct JS emission for the deep-static walker (`skip-static-subtree` class).
//! Skips building `Program` + `print_typed` entirely.

use std::collections::HashMap;

use svelte_ast::attributes::{AttributeValue, ElementAttribute};
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_js_ast::Expression;
use svelte_transform_shared::compile_bump::CompileBump;

use crate::walker::{
    build_inline_template, element_child_is_deep_reactive, fragment_has_deep_reactive,
    rewrite_props_destructured, sanitize_name, serialize_fragment_to_html, trim_pure_whitespace_text,
    ScriptInfo, TextPart,
};

/// Returns true when `fragment` matches the deep-static-walker shape.
pub fn deep_static_walker_eligible(fragment: &Fragment, script: &ScriptInfo) -> bool {
    let nodes = &fragment.nodes;
    if script.async_info.is_some() {
        return false;
    }
    nodes.len() >= 2
        && nodes.iter().all(|n| {
            matches!(
                n,
                FragmentChild::RegularElement(_)
                    | FragmentChild::HtmlTag(_)
                    | FragmentChild::Comment(_)
                    | FragmentChild::Text(_)
            )
        })
        && nodes
            .iter()
            .any(|n| matches!(n, FragmentChild::RegularElement(_)))
        && fragment_has_deep_reactive(fragment)
}

/// Emit client JS for deep-static sparse templates.
pub fn try_emit_deep_static_walker_js(
    root_fragment: &Fragment,
    component_name: &str,
    script: &ScriptInfo,
    bump: &CompileBump,
) -> Option<String> {
    if !deep_static_walker_eligible(root_fragment, script) {
        return None;
    }

    let mut html = String::new();
    let mut needs_import_node = false;
    serialize_fragment_to_html(root_fragment, &mut html, &mut needs_import_node)?;

    let mut out = bump.string();
    emit_prelude(&mut out, script);
    let flag = if needs_import_node { 3 } else { 1 };
    emit_from_html_root(&mut out, &html, flag);

    let mut body = JsBody::new();
    emit_deep_static_body(root_fragment, script, &mut body)?;
    emit_function(&mut out, component_name, script, &body);

    Some(out.into_owned())
}

struct JsBody {
    lines: Vec<String>,
}

impl JsBody {
    fn new() -> Self {
        Self { lines: Vec::new() }
    }

    fn line(&mut self, s: impl Into<String>) {
        self.lines.push(s.into());
    }

    fn stmt(&mut self, s: impl Into<String>) {
        let mut t = s.into();
        if !t.ends_with(';') {
            t.push(';');
        }
        self.lines.push(t);
    }
}

fn emit_prelude(out: &mut svelte_transform_shared::compile_bump::BumpString, script: &ScriptInfo) {
    out.push_str("import 'svelte/internal/disclose-version';\n");
    if script.emit_legacy_flag {
        out.push_str("import 'svelte/internal/flags/legacy';\n");
    }
    out.push_str("import * as $ from 'svelte/internal/client';\n\n");
}

fn emit_from_html_root(out: &mut svelte_transform_shared::compile_bump::BumpString, html: &str, flag: u32) {
    out.push_str("var root = $.from_html(`");
    for ch in html.chars() {
        match ch {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push_str("`, ");
    out.push_str(&flag.to_string());
    out.push_str(");\n\n");
}

fn emit_function(
    out: &mut svelte_transform_shared::compile_bump::BumpString,
    component_name: &str,
    script: &ScriptInfo,
    body: &JsBody,
) {
    out.push_str("export default function ");
    out.push_str(component_name);
    out.push_str("($$anchor");
    if script.uses_props {
        out.push_str(", $$props");
    }
    out.push_str(") {\n");
    for line in &body.lines {
        out.push_str("\t");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("}\n");
}

fn expr_js(e: &Expression) -> String {
    svelte_codegen_js::print_expression_str(e)
}

fn emit_deep_static_body(root_fragment: &Fragment, script: &ScriptInfo, body: &mut JsBody) -> Option<()> {
    let first_is_non_element = root_fragment.nodes.iter().find(|n| match n {
        FragmentChild::Text(t) => !t.data.trim().is_empty(),
        FragmentChild::Comment(_) => true,
        _ => true,
    }).map(|n| !matches!(n, FragmentChild::RegularElement(_))).unwrap_or(false);

    if first_is_non_element {
        body.stmt("$.next()");
    }
    body.line("var fragment = root();");

    let top_elements: Vec<&svelte_ast::elements::RegularElement> = root_fragment
        .nodes
        .iter()
        .filter_map(|n| match n {
            FragmentChild::RegularElement(el) => Some(el),
            _ => None,
        })
        .collect();

    let mut top_element_positions = vec![0usize; top_elements.len()];
    {
        let trimmed = trim_pure_whitespace_text(&root_fragment.nodes);
        let mut pos = 0usize;
        let mut pending_text = false;
        let mut el_idx = 0usize;
        for n in &trimmed {
            match n {
                FragmentChild::Text(_) | FragmentChild::Comment(_) => {
                    if !pending_text {
                        pending_text = true;
                    }
                }
                FragmentChild::RegularElement(_) => {
                    if pending_text {
                        pos += 1;
                        pending_text = false;
                    }
                    if el_idx < top_element_positions.len() {
                        top_element_positions[el_idx] = pos;
                        el_idx += 1;
                    }
                    pos += 1;
                }
                _ => {}
            }
        }
    }

    let mut var_names: HashMap<String, usize> = HashMap::new();
    let mut effects: Vec<(String, Expression)> = Vec::new();
    let mut prev_var: Option<String> = None;
    let mut prev_top_idx: Option<usize> = None;
    let mut first_emitted = false;

    for (i, el) in top_elements.iter().enumerate() {
        if !element_child_is_deep_reactive(el) {
            continue;
        }
        let var = alloc_named(&el.name, &mut var_names);
        let this_pos = top_element_positions[i];
        let init = if !first_emitted {
            if this_pos == 0 {
                "$.first_child(fragment)".to_string()
            } else if this_pos == 1 {
                "$.sibling($.first_child(fragment))".to_string()
            } else {
                format!("$.sibling($.first_child(fragment), {this_pos})")
            }
        } else {
            let prev = prev_var.as_ref()?;
            let prev_idx = prev_top_idx?;
            let offset = this_pos - top_element_positions[prev_idx];
            if offset == 1 {
                format!("$.sibling({prev})")
            } else {
                format!("$.sibling({prev}, {offset})")
            }
        };
        body.line(format!("var {var} = {init};"));
        prev_var = Some(var.clone());
        prev_top_idx = Some(i);
        first_emitted = true;

        apply_reactive_attrs_js(el, &var, body);
        walk_element_interior_js(el, &var, script, body, &mut effects, &mut var_names)?;

        if fragment_has_deep_reactive(&el.fragment) {
            body.stmt(format!("$.reset({var})"));
        }
    }

    // Trailing static top-elements navigation
    if let Some(last_idx) = prev_top_idx {
        let trailing = top_elements.len() - last_idx - 1;
        if trailing > 0 {
            let first_trailing = top_elements[last_idx + 1];
            let var = alloc_named(&first_trailing.name, &mut var_names);
            let prev = prev_var.as_ref()?;
            body.line(format!("var {var} = $.sibling({prev}, 2);"));
            if trailing > 1 {
                body.stmt(format!("$.next({})", (trailing - 1) * 2));
            }
        }
    }

    // template_effect for text bindings
    if effects.len() == 1 {
        let (text_var, expr) = effects.into_iter().next().unwrap();
        let e = expr_js(&expr);
        body.stmt(format!(
            "$.template_effect(() => $.set_text({text_var}, {e}))"
        ));
    } else if effects.len() >= 2 {
        let mut inner = String::from("$.template_effect(() => {\n");
        for (text_var, expr) in effects {
            let e = expr_js(&expr);
            inner.push_str("\t\t$.set_text(");
            inner.push_str(&text_var);
            inner.push_str(", ");
            inner.push_str(&e);
            inner.push_str(");\n");
        }
        inner.push_str("\t})");
        body.stmt(inner);
    }

    body.stmt("$.append($$anchor, fragment)");
    Some(())
}

fn alloc_named(prefix: &str, counts: &mut HashMap<String, usize>) -> String {
    let safe = sanitize_name(prefix);
    let entry = counts.entry(safe.clone()).or_insert(0);
    let n = *entry;
    *entry += 1;
    if n == 0 {
        safe
    } else {
        format!("{safe}_{n}")
    }
}

fn apply_reactive_attrs_js(
    el: &svelte_ast::elements::RegularElement,
    var: &str,
    body: &mut JsBody,
) {
    let is_custom = el.name.contains('-');
    for a in &el.attributes {
        let ElementAttribute::Attribute(attr) = a else {
            continue;
        };
        if is_custom {
            if let AttributeValue::Many(parts) = &attr.value {
                if let Some(svelte_ast::attributes::AttributeValuePart::Text(t)) = parts.first() {
                    body.stmt(format!(
                        "$.set_custom_element_data({}, '{}', '{}')",
                        var,
                        escape_js_single(&attr.name),
                        escape_js_single(&t.data)
                    ));
                }
            }
            continue;
        }
        match attr.name.as_str() {
            "autofocus" => {
                body.stmt(format!("$.autofocus({var}, true)"));
            }
            "muted" if matches!(el.name.as_str(), "source" | "video" | "audio") => {
                body.stmt(format!("{var}.muted = true"));
            }
            "value" if el.name == "option" => {
                if let AttributeValue::Many(parts) = &attr.value {
                    if let Some(svelte_ast::attributes::AttributeValuePart::Text(t)) = parts.first()
                    {
                        let val = escape_js_single(&t.data);
                        body.stmt(format!(
                            "{var}.value = {var}.__value = '{val}'"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

fn walk_element_interior_js(
    el: &svelte_ast::elements::RegularElement,
    parent_var: &str,
    script: &ScriptInfo,
    body: &mut JsBody,
    effects: &mut Vec<(String, Expression)>,
    var_names: &mut HashMap<String, usize>,
) -> Option<()> {
    let raw: Vec<&FragmentChild> = el.fragment.nodes.iter().collect();
    let is_boundary = |n: &&FragmentChild| match n {
        FragmentChild::Text(t) => t.data.trim().is_empty(),
        FragmentChild::Comment(_) => true,
        _ => false,
    };
    let start = raw.iter().position(|n| !is_boundary(n)).unwrap_or(raw.len());
    let end = raw
        .iter()
        .rposition(|n| !is_boundary(n))
        .map(|p| p + 1)
        .unwrap_or(0);
    let children: Vec<&FragmentChild> = raw[start..end].iter().copied().collect();

    let mut reactive_idx: Vec<usize> = Vec::new();
    for (i, c) in children.iter().enumerate() {
        let r = match c {
            FragmentChild::ExpressionTag(_) | FragmentChild::HtmlTag(_) => true,
            FragmentChild::RegularElement(child_el) => element_child_is_deep_reactive(child_el),
            _ => false,
        };
        if r {
            reactive_idx.push(i);
        }
    }
    if reactive_idx.is_empty() {
        return Some(());
    }

    let mut prev_child_var: Option<String> = None;
    let mut prev_child_idx: Option<usize> = None;

    for (k, &i) in reactive_idx.iter().enumerate() {
        let prefix = match children[i] {
            FragmentChild::RegularElement(child_el) => sanitize_name(&child_el.name),
            FragmentChild::HtmlTag(_) => "node".to_string(),
            FragmentChild::ExpressionTag(_) => "text".to_string(),
            _ => "node".to_string(),
        };
        let var = alloc_named(&prefix, var_names);

        if k == 0 {
            let init = if i == 0 {
                format!("$.child({parent_var})")
            } else if i == 1 {
                format!("$.sibling($.child({parent_var}))")
            } else {
                format!("$.sibling($.child({parent_var}), {i})")
            };
            body.line(format!("var {var} = {init};"));
        } else {
            let prev = prev_child_var.as_ref()?;
            let prev_i = prev_child_idx?;
            let offset = i - prev_i;
            let init = if offset == 1 {
                format!("$.sibling({prev})")
            } else {
                format!("$.sibling({prev}, {offset})")
            };
            body.line(format!("var {var} = {init};"));
        }

        match children[i] {
            FragmentChild::HtmlTag(ht) => {
                let expr = rewrite_props_destructured(&ht.expression, &script.props_destructured);
                let e = expr_js(&expr);
                body.stmt(format!("$.html({var}, () => {e})"));
            }
            FragmentChild::RegularElement(child_el) => {
                if child_el.attributes.is_empty() && is_text_only_element_js(child_el) {
                    let text_var = alloc_named("text", var_names);
                    body.line(format!("var {text_var} = $.child({var}, true);"));
                    body.stmt(format!("$.reset({var})"));
                    let mut parts: Vec<TextPart> = Vec::new();
                    for c in &child_el.fragment.nodes {
                        match c {
                            FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                            FragmentChild::ExpressionTag(et) => {
                                parts.push(TextPart::Expr(&et.expression))
                            }
                            _ => return None,
                        }
                    }
                    let inline = build_inline_template(&parts, &script.state_bindings);
                    let inline =
                        rewrite_props_destructured(&inline, &script.props_destructured);
                    effects.push((text_var, inline));
                } else {
                    apply_reactive_attrs_js(child_el, &var, body);
                    if fragment_has_deep_reactive(&child_el.fragment) {
                        body.stmt(format!("$.reset({var})"));
                    }
                }
            }
            FragmentChild::ExpressionTag(et) => {
                let expr = rewrite_props_destructured(&et.expression, &script.props_destructured);
                effects.push((var.clone(), expr));
            }
            _ => {}
        }

        prev_child_var = Some(var);
        prev_child_idx = Some(i);
    }

    if let Some(&last) = reactive_idx.last() {
        let trailing_count = children.len() - 1 - last;
        if trailing_count > 0 {
            body.stmt(format!("$.next({trailing_count})"));
        }
    }
    Some(())
}

fn is_text_only_element_js(el: &svelte_ast::elements::RegularElement) -> bool {
    el.fragment.nodes.iter().all(|n| {
        matches!(
            n,
            FragmentChild::Text(_) | FragmentChild::ExpressionTag(_) | FragmentChild::Comment(_)
        )
    }) && el
        .fragment
        .nodes
        .iter()
        .any(|n| matches!(n, FragmentChild::ExpressionTag(_)))
}

fn escape_js_single(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
