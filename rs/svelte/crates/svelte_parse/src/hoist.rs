//! Post-parse hoist of `<script>` and `<style>` elements out of `Root.fragment`.
//!
//! Mirrors `phases/1-parse/index.js::parse` finalization:
//!
//! - Each `<script>` element becomes a typed `Script` in `Root.instance` or
//!   `Root.module` (depending on `context="module"`). The script body is
//!   reparsed via the OXC bridge to produce a real `svelte_js_ast::Program`.
//! - Each `<style>` element becomes a `StyleSheet` in `Root.css`.
//! - The hoisted elements are removed from the fragment.
//!
//! Mid-fragment scripts/styles (technically invalid) are also hoisted — upstream
//! is lenient here; phase 2 validators handle the diagnostic.

use svelte_ast::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute, FragmentChild,
    RegularElement, Root, Script, ScriptContext};
use svelte_diagnostics::CompileDiagnostic;

use crate::oxc_bridge;
use crate::utils::locator::LineMap;

pub fn hoist_scripts_and_styles(
    root: &mut Root,
    source: &str,
    line_map: &LineMap,
    ts: bool,
) -> Result<(), CompileDiagnostic> {
    let mut i = 0;
    while i < root.fragment.nodes.len() {
        let is_script = matches!(
            &root.fragment.nodes[i],
            FragmentChild::RegularElement(el) if el.name == "script"
        );
        let is_style = matches!(
            &root.fragment.nodes[i],
            FragmentChild::RegularElement(el) if el.name == "style"
        );

        if is_script {
            let el = match root.fragment.nodes.remove(i) {
                FragmentChild::RegularElement(el) => el,
                _ => unreachable!(),
            };
            let is_module = is_module_script(&el.attributes);
            let (script, comments) = build_script_with_comments(&el, source, line_map, ts)?;
            // Append script comments to root.comments for downstream
            // codegen (inter-declarator preservation).
            for c in comments {
                root.comments.push(svelte_ast::root::JsComment {
                    kind: if c.line {
                        svelte_ast::root::JsCommentKind::Line
                    } else {
                        svelte_ast::root::JsCommentKind::Block
                    },
                    value: c.value,
                    start: c.start,
                    end: c.end,
                    loc: svelte_ast::SourceLocation {
                        start: svelte_ast::Position { line: 0, column: 0, character: None },
                        end: svelte_ast::Position { line: 0, column: 0, character: None },
                    },
                });
            }
            if is_module {
                if root.module.is_none() {
                    root.module = Some(script);
                }
            } else if root.instance.is_none() {
                root.instance = Some(script);
            }
            continue;
        }

        if is_style {
            let el = match root.fragment.nodes.remove(i) {
                FragmentChild::RegularElement(el) => el,
                _ => unreachable!(),
            };
            if root.css.is_none() {
                if let Some(sheet) = build_style(&el, source)? {
                    root.css = Some(sheet);
                }
            } else {
                // Second `<style>` block → `style_duplicate` error.
                return Err(svelte_diagnostics::errors::style_duplicate(Some((
                    el.start, el.end,
                ))));
            }
            continue;
        }

        i += 1;
    }
    Ok(())
}

fn build_script_with_comments(
    el: &RegularElement,
    source: &str,
    line_map: &LineMap,
    ts_default: bool,
) -> Result<(Script, Vec<oxc_bridge::RawComment>), CompileDiagnostic> {
    let (body_start, body_end) = body_bounds(el).unwrap_or((el.end as usize, el.end as usize));
    let ts = ts_default
        || attribute_string_value(&el.attributes, "lang")
            .map(|s| s == "ts" || s == "typescript")
            .unwrap_or(false);
    let (content, comments) =
        oxc_bridge::parse_program(source, line_map, body_start, body_end, ts)?;
    let context = if is_module_script(&el.attributes) {
        ScriptContext::Module
    } else {
        ScriptContext::Default
    };
    let attributes: Vec<Attribute> = el
        .attributes
        .iter()
        .filter_map(|a| match a {
            ElementAttribute::Attribute(attr) => Some(attr.clone()),
            _ => None,
        })
        .collect();
    Ok((
        Script {
            start: el.start,
            end: el.end,
            context,
            content,
            attributes,
        },
        comments,
    ))
}

fn build_script(
    el: &RegularElement,
    source: &str,
    line_map: &LineMap,
    ts_default: bool,
) -> Result<Script, CompileDiagnostic> {
    // Body bounds: the Text child written by `read_raw_until_close_tag`.
    let (body_start, body_end) = body_bounds(el).unwrap_or((el.end as usize, el.end as usize));

    // `lang="ts"` flips on TypeScript parsing for this block specifically.
    let ts = ts_default
        || attribute_string_value(&el.attributes, "lang")
            .map(|s| s == "ts" || s == "typescript")
            .unwrap_or(false);

    let (content, _comments) =
        oxc_bridge::parse_program(source, line_map, body_start, body_end, ts)?;
    // Note: comments are captured by `build_script_with_comments` below;
    // callers using `build_script` (legacy entry) drop them.

    let context = if is_module_script(&el.attributes) {
        ScriptContext::Module
    } else {
        ScriptContext::Default
    };

    // Collect attributes as svelte_ast `Attribute`s only (drop directives —
    // `<script>` elements never carry them in practice).
    let attributes: Vec<Attribute> = el
        .attributes
        .iter()
        .filter_map(|a| match a {
            ElementAttribute::Attribute(attr) => Some(attr.clone()),
            _ => None,
        })
        .collect();

    Ok(Script {
        start: el.start,
        end: el.end,
        context,
        content,
        attributes,
    })
}

fn build_style(
    el: &RegularElement,
    source: &str,
) -> Result<Option<svelte_ast::css::StyleSheet>, CompileDiagnostic> {
    let (body_start, _body_end) = match body_bounds(el) {
        Some(b) => b,
        None => return Ok(None),
    };
    let attributes = el.attributes.clone();
    let (sheet, _end) =
        svelte_css_parser::read_style(source, el.start, body_start, attributes, None)?;
    Ok(Some(sheet))
}

fn body_bounds(el: &RegularElement) -> Option<(usize, usize)> {
    let first = el.fragment.nodes.first()?;
    if let FragmentChild::Text(t) = first {
        Some((t.start as usize, t.end as usize))
    } else {
        None
    }
}

fn is_module_script(attrs: &[ElementAttribute]) -> bool {
    attribute_string_value(attrs, "context").as_deref() == Some("module")
        || attrs.iter().any(|a| {
            matches!(
                a,
                ElementAttribute::Attribute(attr)
                    if attr.name == "module" && matches!(attr.value, AttributeValue::Empty)
            )
        })
}

fn attribute_string_value(attrs: &[ElementAttribute], name: &str) -> Option<String> {
    for a in attrs {
        if let ElementAttribute::Attribute(attr) = a {
            if attr.name == name {
                return attribute_value_as_str(&attr.value).map(|s| s.to_string());
            }
        }
    }
    None
}

fn attribute_value_as_str(v: &AttributeValue) -> Option<&str> {
    match v {
        AttributeValue::Empty => None,
        AttributeValue::Single(_) => None,
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                if let AttributeValuePart::Text(t) = &parts[0] {
                    return Some(&t.data);
                }
            }
            None
        }
    }
}
