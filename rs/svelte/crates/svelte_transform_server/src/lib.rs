//! Server-side typed transform.
//!
//! Greenfield rewrite: every output is `svelte_js_ast::Program`, no
//! `serde_json::Value` anywhere. Coverage grows fixture-by-fixture from the
//! simplest static templates outward. Shapes that aren't yet handled return
//! `None` from the entry points; `svelte_compiler::compile` surfaces that as
//! a `typed_server_unsupported` diagnostic.

#![forbid(unsafe_code)]

mod script;
mod typed_fast;

pub use typed_fast::try_typed_server;

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::fragment::FragmentChild;
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

/// Second-tier typed entry point. Currently handles:
/// - "instance script (imports + optionally rune-erasable statements) + simple template"
/// - "single <Component bind:this={x}/>"
/// - "<svelte:element this={tag}>"
pub fn try_typed_server_component(root: &Root, component_name: &str) -> Option<Program> {
    if root.css.is_some() || root.module.is_some() {
        return None;
    }

    // Defer entirely-static fragments to typed_fast (it's cheaper).
    if root.instance.is_none() && is_pure_static_fragment(&root.fragment) {
        return None;
    }

    // Process the instance script: rewrite runes, split imports vs rest.
    let mut script_imports: Vec<Statement> = Vec::new();
    let mut script_rest: Vec<Statement> = Vec::new();
    let mut uses_props = false;
    if let Some(s) = root.instance.as_ref() {
        let mut content = s.content.clone();
        uses_props = script::rewrite_program_for_server(&mut content);
        let (imports, rest) = partition_imports(&content.body)?;
        script_imports = imports;
        script_rest = rest;
    }

    // Build the function body: rune-rewritten script statements first, then template.
    let mut func_body: Vec<Statement> = script_rest;
    let template_body = lower_fragment_server(&root.fragment)?;
    func_body.extend(template_body);

    // Build the parameter list. Runes-mode uses_props adds $$props.
    let mut params = vec![t::pat_id("$$renderer")];
    if uses_props {
        params.push(t::pat_id("$$props"));
    }

    let mut top: Vec<Statement> = Vec::with_capacity(2 + script_imports.len());
    top.push(t::import_namespace("$", "svelte/internal/server"));
    top.extend(script_imports);
    top.push(t::export_default_function(component_name, params, func_body));
    Some(t::program(top))
}

/// Lower an entire fragment to a sequence of server-side statements.
///
/// Walks `f.nodes` collecting static + dynamic content into a rolling
/// template literal that's pushed via `$$renderer.push(\`...\`)`. Nodes
/// that don't fit (Component, SvelteElement, blocks) flush the buffer
/// and emit their own statement.
fn lower_fragment_server(f: &svelte_ast::fragment::Fragment) -> Option<Vec<Statement>> {
    let mut out = Vec::new();
    let mut buf = TemplateBuf::new();
    for n in &f.nodes {
        if append_node_to_template(n, &mut buf).is_none() {
            // Flush + emit non-template node.
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            match n {
                FragmentChild::Component(c) => out.push(lower_component_server(c)?),
                FragmentChild::SvelteElement(el) => out.push(lower_svelte_element_server(el)?),
                _ => return None,
            }
        }
    }
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    }
    Some(out)
}

/// Append a fragment child to the template literal buffer. Returns `None`
/// if the node can't be expressed inline (Component, block, etc.).
fn append_node_to_template(n: &FragmentChild, buf: &mut TemplateBuf) -> Option<()> {
    match n {
        FragmentChild::Text(t) => {
            buf.push_str(&escape_text(&t.data));
            Some(())
        }
        FragmentChild::ExpressionTag(tag) => {
            // `${$.escape(expr)}`
            buf.push_expr(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "escape"),
                arguments: vec![Argument::Expression(tag.expression.clone())],
                optional: false,
                span: Span::ZERO,
            })));
            Some(())
        }
        FragmentChild::RegularElement(el) if el.attributes.is_empty() => {
            // Simple element with no attributes — serialize verbatim.
            buf.push_str("<");
            buf.push_str(&el.name);
            buf.push_str(">");
            for c in &el.fragment.nodes {
                append_node_to_template(c, buf)?;
            }
            if !is_void(&el.name) {
                buf.push_str("</");
                buf.push_str(&el.name);
                buf.push_str(">");
            }
            Some(())
        }
        FragmentChild::Comment(_) => Some(()), // HTML comments dropped server-side
        _ => None,
    }
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}

/// Buffer for accumulating `$$renderer.push(\`...\`)` template content.
struct TemplateBuf {
    /// Static string chunks separated by expression placeholders. Always
    /// has `exprs.len() + 1` entries when non-empty.
    parts: Vec<String>,
    exprs: Vec<Expression>,
}

impl TemplateBuf {
    fn new() -> Self {
        Self { parts: vec![String::new()], exprs: Vec::new() }
    }

    fn push_str(&mut self, s: &str) {
        self.parts.last_mut().unwrap().push_str(s);
    }

    fn push_expr(&mut self, e: Expression) {
        self.exprs.push(e);
        self.parts.push(String::new());
    }

    fn is_empty(&self) -> bool {
        self.exprs.is_empty() && self.parts.iter().all(|s| s.is_empty())
    }

    /// True if the buffer carries nothing but whitespace static text (no
    /// expressions). Used to skip emitting a `push(\`   \`)` for a fragment
    /// whose only content was whitespace between non-template-fits nodes.
    fn is_whitespace_only(&self) -> bool {
        self.exprs.is_empty() && self.parts.iter().all(|s| s.chars().all(|c| c.is_whitespace()))
    }

    /// Flush to a `$$renderer.push(\`...\`)` statement (or None when empty
    /// or whitespace-only). Leading whitespace in parts[0] and trailing
    /// whitespace in parts.last() are trimmed (matches upstream's
    /// `preserveWhitespace: false` boundary collapse).
    fn flush(&mut self) -> Option<Statement> {
        if self.is_empty() || self.is_whitespace_only() {
            // Reset buffer state and drop the whitespace.
            self.parts.clear();
            self.parts.push(String::new());
            self.exprs.clear();
            return None;
        }
        // Trim leading whitespace from the first chunk.
        if let Some(first) = self.parts.first_mut() {
            *first = first.trim_start().to_string();
        }
        // Trim trailing whitespace from the last chunk.
        if let Some(last) = self.parts.last_mut() {
            *last = last.trim_end().to_string();
        }
        let parts = std::mem::take(&mut self.parts);
        let exprs = std::mem::take(&mut self.exprs);
        self.parts.push(String::new());
        Some(t::stmt(Expression::Call(Box::new(CallExpression {
            callee: t::member_id(t::id("$$renderer"), "push"),
            arguments: vec![Argument::Expression(t::template_raw(parts, exprs))],
            optional: false,
            span: Span::ZERO,
        }))))
    }
}

/// `<svelte:element this={tag}>` → `$.element($$renderer, tag);` (server form,
/// drops attribute content for now).
fn lower_svelte_element_server(
    el: &svelte_ast::elements::SvelteElement,
) -> Option<Statement> {
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$"), "element"),
        arguments: vec![
            Argument::Expression(t::id("$$renderer")),
            Argument::Expression(el.tag.clone()),
        ],
        optional: false,
        span: Span::ZERO,
    }))))
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

#[allow(dead_code)]
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

fn is_pure_static_fragment(f: &svelte_ast::fragment::Fragment) -> bool {
    fn is_static(n: &FragmentChild) -> bool {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => true,
            FragmentChild::RegularElement(el) => {
                el.attributes.is_empty() && el.fragment.nodes.iter().all(is_static)
            }
            _ => false,
        }
    }
    f.nodes.iter().all(is_static)
}

#[allow(dead_code)]
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
