//! Server-side typed transform.
//!
//! Greenfield rewrite: every output is `svelte_js_ast::Program`, no
//! `serde_json::Value` anywhere. Coverage grows fixture-by-fixture from the
//! simplest static templates outward. Shapes that aren't yet handled return
//! `None` from the entry points; `svelte_compiler::compile` surfaces that as
//! a `typed_server_unsupported` diagnostic.

#![forbid(unsafe_code)]

mod typed_fast;

pub use typed_fast::try_typed_server;

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::fragment::FragmentChild;
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

/// Second-tier typed entry point. Currently handles:
/// - "instance script with imports only + empty template"
/// - "no script + single static-template component" (e.g. `<Foo bind:this={x}/>`)
pub fn try_typed_server_component(root: &Root, component_name: &str) -> Option<Program> {
    if root.css.is_some() {
        return None;
    }
    if root.module.is_some() {
        return None;
    }

    // Decide whether typed_fast already claims this shape.
    if root.instance.is_none() && root.module.is_none() && !fragment_is_empty(&root.fragment) {
        // Only handle the single-component case here; pure static HTML is
        // typed_fast's territory.
        let only = single_non_ws_node(&root.fragment)?;
        if !matches!(only, FragmentChild::Component(_)) {
            return None;
        }
    }

    let (script_imports, script_body) = match root.instance.as_ref() {
        Some(s) => partition_imports(&s.content.body)?,
        None => (Vec::new(), Vec::new()),
    };
    if !script_body.is_empty() {
        return None;
    }

    // Build the function body from the template.
    let func_body = lower_fragment_server(&root.fragment)?;

    let mut top: Vec<Statement> = Vec::with_capacity(2 + script_imports.len());
    top.push(t::import_namespace("$", "svelte/internal/server"));
    top.extend(script_imports);
    top.push(t::export_default_function(
        component_name,
        vec![t::pat_id("$$renderer")],
        func_body,
    ));
    Some(t::program(top))
}

/// Lower an entire fragment to a sequence of server-side statements.
fn lower_fragment_server(f: &svelte_ast::fragment::Fragment) -> Option<Vec<Statement>> {
    let mut out = Vec::new();
    for n in &f.nodes {
        match n {
            FragmentChild::Text(t) => {
                if !t.data.trim().is_empty() {
                    return None;
                }
            }
            FragmentChild::Component(c) => out.push(lower_component_server(c)?),
            _ => return None,
        }
    }
    Some(out)
}

/// `<Foo a={x} b="y" {...rest} />` → `Foo($$renderer, { a: x, b: 'y', ...rest });`.
/// Directives (`bind:this`, `on:click`, etc.) are dropped server-side.
fn lower_component_server(c: &svelte_ast::elements::Component) -> Option<Statement> {
    let mut props: Vec<ObjectMember> = Vec::new();
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(a) => {
                if let Some(prop) = attribute_to_object_member(a) {
                    props.push(prop);
                } else {
                    return None;
                }
            }
            ElementAttribute::SpreadAttribute(s) => {
                props.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            // All other directives (bind/use/transition/animate/let/class/style/on/attach)
            // are SSR-irrelevant — drop them.
            _ => {}
        }
    }
    let args = vec![
        Argument::Expression(t::id("$$renderer")),
        Argument::Expression(Expression::Object(Box::new(ObjectExpression {
            properties: props,
            span: Span::ZERO,
        }))),
    ];
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::id(&c.name),
        arguments: args,
        optional: false,
        span: Span::ZERO,
    }))))
}

/// `name={expr}` → `{ name: expr }`. `name="literal"` → `{ name: 'literal' }`.
fn attribute_to_object_member(a: &Attribute) -> Option<ObjectMember> {
    let value: Expression = match &a.value {
        AttributeValue::Empty => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: true,
            span: Span::ZERO,
        }))),
        AttributeValue::Single(tag) => tag.expression.clone(),
        AttributeValue::Many(parts) => {
            // For now, only support single-part Text or single-part Expression.
            if parts.len() == 1 {
                match &parts[0] {
                    AttributeValuePart::Text(t) => {
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: t.data.clone(),
                            raw: Some(format!("'{}'", t.raw.replace('\'', "\\'"))),
                            span: Span::ZERO,
                        })))
                    }
                    AttributeValuePart::ExpressionTag(e) => e.expression.clone(),
                }
            } else {
                // TODO: concatenated parts → template literal.
                return None;
            }
        }
    };
    Some(ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: a.name.clone(),
            span: Span::ZERO,
        }),
        value,
        kind: PropertyKind::Init,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    })))
}

fn single_non_ws_node(f: &svelte_ast::fragment::Fragment) -> Option<&FragmentChild> {
    let non_ws: Vec<&FragmentChild> = f
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if non_ws.len() == 1 {
        Some(non_ws[0])
    } else {
        None
    }
}

fn fragment_is_empty(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().all(|n| match n {
        FragmentChild::Text(t) => t.data.trim().is_empty(),
        _ => false,
    })
}

fn partition_imports(body: &[Statement]) -> Option<(Vec<Statement>, Vec<Statement>)> {
    let mut imports = Vec::new();
    let mut rest = Vec::new();
    let mut saw_non_import = false;
    for s in body {
        match s {
            Statement::Import(_) => {
                if saw_non_import {
                    return None;
                }
                imports.push(s.clone());
            }
            _ => {
                saw_non_import = true;
                rest.push(s.clone());
            }
        }
    }
    Some((imports, rest))
}
