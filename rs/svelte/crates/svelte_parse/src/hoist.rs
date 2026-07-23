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

use bumpalo::Bump;
use svelte_ast::{
    Attribute, AttributeValue, AttributeValuePart, ElementAttribute, FragmentChild,
    RegularElement, Root, Script, ScriptContext,
};
use svelte_diagnostics::CompileDiagnostic;

use oxc_allocator::Allocator;

use crate::oxc_bridge;
use crate::utils::locator::LineMap;

pub fn hoist_scripts_and_styles<'a>(
    alloc: &mut Allocator,
    bump: &'a Bump,
    root: &mut Root<'a>,
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
            let (script, comments) =
                build_script_with_comments(alloc, bump, el, source, line_map, ts)?;
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
                if let Some(sheet) = build_style(el, source)? {
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

fn build_script_with_comments<'a>(
    alloc: &mut Allocator,
    bump: &'a Bump,
    el: RegularElement<'a>,
    source: &str,
    line_map: &LineMap,
    ts_default: bool,
) -> Result<(Script<'a>, Vec<oxc_bridge::RawComment>), CompileDiagnostic> {
    let (body_start, body_end) = body_bounds(&el).unwrap_or((el.end as usize, el.end as usize));
    let ts = ts_default
        || attribute_string_value(&el.attributes, "lang")
            .map(|s| s == "ts" || s == "typescript")
            .unwrap_or(false);
    let (content, comments) =
        oxc_bridge::parse_program(alloc, bump, source, line_map, body_start, body_end, ts)?;
    let context = if is_module_script(&el.attributes) {
        ScriptContext::Module
    } else {
        ScriptContext::Default
    };
    let attributes = script_attributes(bump, el.attributes);
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

fn build_style<'a>(
    el: RegularElement<'a>,
    source: &str,
) -> Result<Option<svelte_ast::css::StyleSheet<'a>>, CompileDiagnostic> {
    let (body_start, _body_end) = match body_bounds(&el) {
        Some(b) => b,
        None => return Ok(None),
    };
    let start = el.start;
    let attributes: Vec<_> = el.attributes.into_iter().collect();
    let (sheet, _end) =
        svelte_css_parser::read_style(source, start, body_start, attributes, None)?;
    Ok(Some(sheet))
}

fn script_attributes<'a>(
    bump: &'a Bump,
    attrs: bumpalo::collections::Vec<'a, ElementAttribute<'a>>,
) -> bumpalo::collections::Vec<'a, Attribute<'a>> {
    let mut out = bumpalo::collections::Vec::new_in(bump);
    for a in attrs.into_iter() {
        if let ElementAttribute::Attribute(attr) = a {
            out.push(attr);
        }
    }
    out
}

fn body_bounds<'a>(el: &RegularElement<'a>) -> Option<(usize, usize)> {
    let first = el.fragment.nodes.first()?;
    if let FragmentChild::Text(t) = first {
        Some((t.start as usize, t.end as usize))
    } else {
        None
    }
}

fn is_module_script<'a>(attrs: &[ElementAttribute<'a>]) -> bool {
    attribute_string_value(attrs, "context").as_deref() == Some("module")
        || attrs.iter().any(|a| {
            matches!(
                a,
                ElementAttribute::Attribute(attr)
                    if attr.name == "module" && matches!(attr.value, AttributeValue::Empty)
            )
        })
}

fn attribute_string_value<'a>(attrs: &[ElementAttribute<'a>], name: &str) -> Option<String> {
    for a in attrs {
        if let ElementAttribute::Attribute(attr) = a {
            if attr.name == name {
                return attribute_value_as_str(&attr.value).map(|s| s.to_string());
            }
        }
    }
    None
}

fn attribute_value_as_str<'a>(v: &'a AttributeValue<'a>) -> Option<&'a str> {
    match v {
        AttributeValue::Empty => None,
        AttributeValue::Single(_) => None,
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                if let AttributeValuePart::Text(t) = &parts[0] {
                    return Some(t.data.as_str());
                }
            }
            None
        }
    }
}
