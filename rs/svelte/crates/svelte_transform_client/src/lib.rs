//! Client-side typed transform.
//!
//! Greenfield rewrite: every output is `svelte_js_ast::Program`, no
//! `serde_json::Value` anywhere. Coverage grows fixture-by-fixture from the
//! simplest static templates outward. Shapes that aren't yet handled return
//! `None` from the entry points; `svelte_compiler::compile` surfaces that as
//! a `typed_client_unsupported` diagnostic.

#![forbid(unsafe_code)]

mod deep_static_js;
mod direct_codegen;
#[cfg(test)]
mod deep_static_js_tests;
#[cfg(test)]
mod direct_codegen_tests;
mod script_fast;
mod sparse_compile;
mod sparse_multi_if_js;
mod sparse_pipeline;
mod static_html_cache;
mod typed_fast;
mod walker;

pub use direct_codegen::{
    try_emit_client_program_direct, try_emit_fully_static_client_js,
};
pub use deep_static_js::try_emit_deep_static_walker_js;
pub use sparse_compile::try_emit_sparse_islands_client_js;
pub use static_html_cache::precompute_static_html_cache;
pub use sparse_pipeline::try_sparse_islands_program;

pub use typed_fast::try_typed_client;
pub use walker::try_typed_client_walker;
pub use walker::try_typed_client_walker_with;
pub use walker::try_typed_client_walker_with_filename;
pub use walker::fold_in_fragment as walker_fold_in_fragment;

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::fragment::FragmentChild;
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;
use std::borrow::Cow;

/// Compile options threaded through from `svelte_compiler::CompileOptions`.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub filename: Option<String>,
    pub dev: bool,
    pub hmr: bool,
    pub experimental_async: bool,
    pub fragments: FragmentsMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FragmentsMode {
    #[default]
    Html,
    Tree,
}

/// Second-tier typed entry point. Currently handles:
/// - "instance script with imports only + empty template"
/// - "no script + single <Component bind:this={x}>" (bind-this client)
pub fn try_typed_client_component(root: &Root<'_>, component_name: &str) -> Option<Program> {
    if root.css.is_some() || root.module.is_some() {
        return None;
    }

    let fragment_empty = fragment_is_empty(&root.fragment);

    // Determine fragment shape for routing.
    let single_component = if !fragment_empty {
        match single_non_ws_node(&root.fragment)? {
            FragmentChild::Component(c) => Some(c),
            _ => return None,
        }
    } else {
        None
    };

    let (script_imports, script_body) = match root.instance.as_ref() {
        Some(s) => partition_imports(&s.content.body)?,
        None => (Vec::new(), Vec::new()),
    };
    if !script_body.is_empty() {
        return None;
    }

    // Build function body from template.
    let func_body = if let Some(c) = single_component {
        lower_single_component_client(&c)?
    } else {
        Vec::new()
    };

    let mut top: Vec<Statement> = Vec::with_capacity(4 + script_imports.len());
    top.push(t::import_side_effect("svelte/internal/disclose-version"));
    // Non-runes mode emits `flags/legacy` too.
    let uses_runes = false; // None of the supported shapes here use runes.
    if !uses_runes {
        top.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    top.push(t::import_namespace("$", "svelte/internal/client"));
    top.extend(script_imports);
    top.push(t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
        func_body,
    ));
    Some(t::program(top))
}

/// `<Foo bind:this={x} a={y} {...rest} />` →
/// either `Foo($$anchor, {...props})` (no bind:this) or
/// `$.bind_this(Foo($$anchor, {...props}), ($$value) => x = $$value, () => x)`.
fn lower_single_component_client(
    c: &svelte_ast::elements::Component<'_>,
) -> Option<Vec<Statement>> {
    let mut props: Vec<ObjectMember> = Vec::new();
    let mut bind_this: Option<Expression> = None;
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(a) => {
                props.push(attribute_to_object_member(a)?);
            }
            ElementAttribute::SpreadAttribute(s) => {
                props.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            ElementAttribute::BindDirective(b) if b.name == "this" => {
                bind_this = Some(b.expression.clone());
            }
            // Other directives not yet supported in this minimal entry point.
            _ => return None,
        }
    }

    // Non-runes mode adds `$$legacy: true` to the props.
    props.push(ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Borrowed("$$legacy"),
            span: Span::ZERO,
        }),
        value: Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: true,
            span: Span::ZERO,
        }))),
        kind: PropertyKind::Init,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    })));

    // Build the component-call expression.
    let component_call = Expression::Call(Box::new(CallExpression {
        callee: t::id_owned(c.name.to_string()),
        arguments: vec![
            Argument::Expression(t::id("$$anchor")),
            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                properties: props,
                span: Span::ZERO,
            }))),
        ],
        optional: false,
        span: Span::ZERO,
    }));

    let stmt = match bind_this {
        Some(target_expr) => {
            // `$.bind_this(Foo(...), ($$value) => target = $$value, () => target)`
            let setter = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: vec![t::pat_id("$$value")],
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(Expression::Assignment(Box::new(
                    AssignmentExpression {
                        left: AssignmentTarget::Pattern(expr_to_pattern(&target_expr)?),
                        operator: AssignmentOperator::Assign,
                        right: t::id("$$value"),
                        span: Span::ZERO,
                    },
                ))),
                r#async: false,
                span: Span::ZERO,
            }));
            let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(target_expr),
                r#async: false,
                span: Span::ZERO,
            }));
            t::stmt(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "bind_this"),
                arguments: vec![
                    Argument::Expression(component_call),
                    Argument::Expression(setter),
                    Argument::Expression(getter),
                ],
                optional: false,
                span: Span::ZERO,
            })))
        }
        None => t::stmt(component_call),
    };
    Some(vec![stmt])
}

fn expr_to_pattern(e: &Expression) -> Option<Pattern> {
    match e {
        Expression::Identifier(i) => Some(Pattern::Identifier(i.clone())),
        Expression::Member(m) => Some(Pattern::Member(m.clone())),
        _ => None,
    }
}

fn fragment_is_empty(f: &svelte_ast::fragment::Fragment<'_>) -> bool {
    f.nodes.iter().all(|n| match n {
        FragmentChild::Text(t) => t.data.trim().is_empty(),
        _ => false,
    })
}

fn single_non_ws_node<'a>(
    f: &'a svelte_ast::fragment::Fragment<'a>,
) -> Option<&'a FragmentChild<'a>> {
    let non_ws: Vec<&FragmentChild<'_>> = f
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

fn attribute_to_object_member(a: &Attribute<'_>) -> Option<ObjectMember> {
    let value: Expression = match &a.value {
        AttributeValue::Empty => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: true,
            span: Span::ZERO,
        }))),
        AttributeValue::Single(tag) => tag.expression.clone(),
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                match &parts[0] {
                    AttributeValuePart::Text(t) => {
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(t.data.to_string()),
                            raw: Some(format!("'{}'", t.raw.replace('\'', "\\'"))),
                            span: Span::ZERO,
                        })))
                    }
                    AttributeValuePart::ExpressionTag(e) => e.expression.clone(),
                }
            } else {
                return None;
            }
        }
    };
    Some(ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Owned(a.name.to_string()),
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
