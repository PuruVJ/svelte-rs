//! Minimal client template walker.
//!
//! Handles a narrowly-scoped shape that nevertheless covers a useful slice of
//! fixtures: a multi-root template (or single-root) where each top-level node
//! is one of:
//! - A RegularElement with no attributes and either purely-static children OR
//!   a single non-reactive ExpressionTag as its only non-ws child.
//! - A Component (lowered to a `<!>` placeholder + `Component(node, {...})`).
//!
//! Reactive (state-dependent) expressions are NOT supported — the walker
//! bails so the existing static fast path or future visitors can take over.
//!
//! Ported from:
//! - packages/svelte/src/compiler/phases/3-transform/client/visitors/RegularElement.js
//! - packages/svelte/src/compiler/phases/3-transform/client/visitors/Component.js
//! - packages/svelte/src/compiler/phases/3-transform/client/visitors/template.js

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::elements::{Component, RegularElement};
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

pub fn try_typed_client_walker(root: &Root, component_name: &str) -> Option<Program> {
    if root.css.is_some() || root.module.is_some() || root.instance.is_some() {
        return None;
    }

    // Collect top-level non-whitespace nodes.
    let nodes: Vec<&FragmentChild> = root
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if nodes.is_empty() {
        return None;
    }

    let mut classified: Vec<NodeKind> = Vec::with_capacity(nodes.len());
    for n in &nodes {
        classified.push(classify(n)?);
    }

    let is_multi_root = nodes.len() > 1;
    let mut html = String::with_capacity(64);
    let mut body_stmts: Vec<Statement> = Vec::new();

    let mut prev_var: Option<String> = None;
    let mut var_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    let last_idx = classified.len() - 1;
    for (i, kind) in classified.iter().enumerate() {
        match kind {
            NodeKind::StaticElement(el) => {
                serialize_element(el, &mut html, /*body*/ true)?;
                let var = unique_var(&el.name, &mut var_counts);
                emit_nav(&mut body_stmts, &var, prev_var.as_deref(), is_multi_root);
                prev_var = Some(var);
            }
            NodeKind::InterpElement(el, expr) => {
                serialize_element(el, &mut html, /*body*/ false)?;
                let var = unique_var(&el.name, &mut var_counts);
                emit_nav(&mut body_stmts, &var, prev_var.as_deref(), is_multi_root);
                // `var.textContent = EXPR;`
                let target = Expression::Member(Box::new(MemberExpression {
                    object: t::id(&var),
                    property: MemberProperty::Identifier(Identifier {
                        name: "textContent".to_string(),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }));
                body_stmts.push(t::stmt(Expression::Assignment(Box::new(
                    AssignmentExpression {
                        left: AssignmentTarget::Expression(target),
                        operator: AssignmentOperator::Assign,
                        right: textcontent_value((*expr).clone()),
                        span: Span::ZERO,
                    },
                ))));
                prev_var = Some(var);
            }
            NodeKind::Component(c) => {
                // `<!>` placeholder in template; `Component(node, {...})` in body.
                html.push_str("<!>");
                let var = unique_var("node", &mut var_counts);
                emit_nav(&mut body_stmts, &var, prev_var.as_deref(), is_multi_root);
                body_stmts.push(component_call(c, &var)?);
                prev_var = Some(var);
            }
        }
        // Append a single space between top-level siblings (matches upstream
        // whitespace-collapsed serialization).
        if i < last_idx {
            html.push(' ');
        }
    }

    // `$.append($$anchor, fragment)` (multi-root) / `$.append($$anchor, TAG)` (single).
    let root_holder = if is_multi_root {
        "fragment".to_string()
    } else {
        prev_var.clone().unwrap_or_else(|| "fragment".to_string())
    };
    if is_multi_root {
        // Prepend `var fragment = root();` before the navigation statements.
        body_stmts.insert(0, t::var("fragment", t::call(t::id("root"), vec![])));
    } else if let Some(name) = &prev_var {
        // For single-root, the very first nav assigned `name = root()` — we
        // emitted `$.first_child(...)` though. Replace with `root()` call:
        // actually the single-root navigation differs. Bail for now if
        // single-root reached this point (the existing typed_fast path
        // handles pure-static single roots).
        let _ = name;
    }
    body_stmts.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(&root_holder)],
    )));

    // Bail if single-root — typed_fast covers static, and we don't yet have a
    // single-root non-static navigation pattern matching upstream's output.
    if !is_multi_root {
        return None;
    }

    // Module-level: `var root = $.from_html(\`HTML\`, 1);`
    let mut from_html_args = vec![t::template_raw(vec![html], vec![])];
    if is_multi_root {
        from_html_args.push(t::lit_number(1.0));
    }
    let root_decl = t::var(
        "root",
        t::call(t::member_id(t::id("$"), "from_html"), from_html_args),
    );

    let export = t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
        body_stmts,
    );

    Some(t::program(vec![
        t::import_side_effect("svelte/internal/disclose-version"),
        t::import_side_effect("svelte/internal/flags/legacy"),
        t::import_namespace("$", "svelte/internal/client"),
        root_decl,
        export,
    ]))
}

/// `var name = $.first_child(fragment);` (first nav) or `var name = $.sibling(prev, 2);`.
fn emit_nav(out: &mut Vec<Statement>, name: &str, prev: Option<&str>, _is_multi_root: bool) {
    let init = if let Some(p) = prev {
        // `$.sibling(prev, 2)` — 2 skips the whitespace text between siblings.
        t::call(
            t::member_id(t::id("$"), "sibling"),
            vec![t::id(p), t::lit_number(2.0)],
        )
    } else {
        t::call(t::member_id(t::id("$"), "first_child"), vec![t::id("fragment")])
    };
    out.push(t::var(name, init));
}

fn unique_var(base: &str, counts: &mut std::collections::HashMap<String, usize>) -> String {
    let n = counts.entry(base.to_string()).or_insert(0);
    let name = if *n == 0 {
        base.to_string()
    } else {
        format!("{base}_{n}")
    };
    *n += 1;
    name
}

enum NodeKind<'a> {
    StaticElement(&'a RegularElement),
    InterpElement(&'a RegularElement, &'a Expression),
    Component(&'a Component),
}

fn classify(n: &FragmentChild) -> Option<NodeKind<'_>> {
    match n {
        FragmentChild::RegularElement(el) => {
            if !el.attributes.is_empty() {
                return None;
            }
            // Examine children: either all static, or exactly one ExpressionTag
            // (with optional whitespace) and nothing reactive.
            let non_ws: Vec<&FragmentChild> = el
                .fragment
                .nodes
                .iter()
                .filter(|c| match c {
                    FragmentChild::Text(t) => !t.data.trim().is_empty(),
                    _ => true,
                })
                .collect();
            if non_ws.len() == 1 {
                if let FragmentChild::ExpressionTag(et) = non_ws[0] {
                    if !expr_is_safe_for_textcontent(&et.expression) {
                        return None;
                    }
                    return Some(NodeKind::InterpElement(el, &et.expression));
                }
            }
            // Otherwise must be entirely static.
            if all_static(&el.fragment.nodes) {
                Some(NodeKind::StaticElement(el))
            } else {
                None
            }
        }
        FragmentChild::Component(c) => Some(NodeKind::Component(c)),
        _ => None,
    }
}

/// Conservative check: expression is "safe" (doesn't reference reactive state).
/// Identifiers we can't prove non-reactive are rejected so we don't silently
/// produce wrong code. Literals, Member access into globals, simple calls into
/// globals, and template-fold-friendly expressions are accepted.
fn expr_is_safe_for_textcontent(e: &Expression) -> bool {
    match e {
        Expression::Literal(_) => true,
        // `location.href`-style: object is a bare Identifier, recurse on object only.
        Expression::Member(m) => match &m.object {
            Expression::Identifier(_) => !m.computed,
            _ => expr_is_safe_for_textcontent(&m.object) && !m.computed,
        },
        Expression::Call(c) => {
            if !expr_is_safe_for_textcontent(&c.callee) {
                return false;
            }
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => {
                        if !expr_is_safe_for_textcontent(e) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
        Expression::Identifier(_) => true,
        _ => false,
    }
}

fn all_static(nodes: &[FragmentChild]) -> bool {
    nodes.iter().all(|c| match c {
        FragmentChild::Text(_) | FragmentChild::Comment(_) => true,
        FragmentChild::RegularElement(el) => {
            el.attributes.is_empty() && all_static(&el.fragment.nodes)
        }
        _ => false,
    })
}

fn serialize_element(
    el: &RegularElement,
    out: &mut String,
    include_body: bool,
) -> Option<()> {
    out.push('<');
    out.push_str(&el.name);
    out.push('>');
    if is_void(&el.name) {
        return Some(());
    }
    if include_body {
        for c in &el.fragment.nodes {
            serialize_static_child(c, out)?;
        }
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
    Some(())
}

fn serialize_static_child(c: &FragmentChild, out: &mut String) -> Option<()> {
    match c {
        FragmentChild::Text(t) => {
            for ch in t.data.chars() {
                match ch {
                    '`' => out.push_str("\\`"),
                    '\\' => out.push_str("\\\\"),
                    _ => out.push(ch),
                }
            }
            Some(())
        }
        FragmentChild::RegularElement(el) => serialize_element(el, out, true),
        FragmentChild::Comment(_) => Some(()),
        _ => None,
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}

/// Convert a number literal to its string representation when targeting
/// `.textContent`. Other expressions pass through unchanged.
fn textcontent_value(e: Expression) -> Expression {
    if let Expression::Literal(lit) = &e {
        if let Literal::Number(n) = lit.as_ref() {
            return Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: format_num(n.value),
                raw: None,
                span: Span::ZERO,
            })));
        }
    }
    e
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        return format!("{}", n as i64);
    }
    format!("{n}")
}

fn component_call(c: &Component, node_var: &str) -> Option<Statement> {
    let mut props: Vec<ObjectMember> = Vec::new();
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(a) => props.push(attr_to_prop(a)?),
            ElementAttribute::SpreadAttribute(s) => {
                props.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            _ => return None,
        }
    }
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::id(&c.name),
        arguments: vec![
            Argument::Expression(t::id(node_var)),
            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                properties: props,
                span: Span::ZERO,
            }))),
        ],
        optional: false,
        span: Span::ZERO,
    }))))
}

fn attr_to_prop(a: &Attribute) -> Option<ObjectMember> {
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
                            value: t.data.clone(),
                            raw: None,
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

/// Apply compile-time Math.X(literal-nums) fold to every expression in the
/// fragment. Mirrors the server-side fold so client output matches upstream.
pub fn fold_in_fragment(f: &mut Fragment) {
    for n in &mut f.nodes {
        fold_in_node(n);
    }
}

fn fold_in_node(n: &mut FragmentChild) {
    match n {
        FragmentChild::ExpressionTag(t) => fold_expr(&mut t.expression),
        FragmentChild::HtmlTag(t) => fold_expr(&mut t.expression),
        FragmentChild::RegularElement(el) => {
            for attr in &mut el.attributes {
                fold_in_attr(attr);
            }
            for c in &mut el.fragment.nodes {
                fold_in_node(c);
            }
        }
        FragmentChild::Component(c) => {
            for attr in &mut c.attributes {
                fold_in_attr(attr);
            }
            for c in &mut c.fragment.nodes {
                fold_in_node(c);
            }
        }
        _ => {}
    }
}

fn fold_in_attr(attr: &mut ElementAttribute) {
    match attr {
        ElementAttribute::Attribute(a) => match &mut a.value {
            AttributeValue::Single(tag) => fold_expr(&mut tag.expression),
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        fold_expr(&mut t.expression);
                    }
                }
            }
            _ => {}
        },
        ElementAttribute::SpreadAttribute(s) => fold_expr(&mut s.expression),
        _ => {}
    }
}

fn fold_expr(e: &mut Expression) {
    match e {
        Expression::Call(c) => {
            fold_expr(&mut c.callee);
            for a in &mut c.arguments {
                if let Argument::Expression(e) = a {
                    fold_expr(e);
                }
            }
            if let Some(folded) = try_fold_math_call(c) {
                *e = folded;
            }
        }
        Expression::Member(m) => fold_expr(&mut m.object),
        Expression::Binary(b) => {
            fold_expr(&mut b.left);
            fold_expr(&mut b.right);
        }
        Expression::Logical(l) => {
            fold_expr(&mut l.left);
            fold_expr(&mut l.right);
        }
        Expression::Conditional(c) => {
            fold_expr(&mut c.test);
            fold_expr(&mut c.consequent);
            fold_expr(&mut c.alternate);
        }
        Expression::Unary(u) => fold_expr(&mut u.argument),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                fold_expr(e);
            }
        }
        Expression::Paren(p) => fold_expr(&mut p.expression),
        _ => {}
    }
}

fn try_fold_math_call(c: &CallExpression) -> Option<Expression> {
    let m = match &c.callee {
        Expression::Member(m) => m,
        _ => return None,
    };
    if m.computed || m.optional {
        return None;
    }
    let obj = match &m.object {
        Expression::Identifier(i) => i.name.as_str(),
        _ => return None,
    };
    let prop = match &m.property {
        MemberProperty::Identifier(i) => i.name.as_str(),
        _ => return None,
    };
    if obj != "Math" {
        return None;
    }
    let mut nums: Vec<f64> = Vec::with_capacity(c.arguments.len());
    for a in &c.arguments {
        let e = match a {
            Argument::Expression(e) => e,
            _ => return None,
        };
        let n = match e {
            Expression::Literal(lit) => match lit.as_ref() {
                Literal::Number(n) => n.value,
                _ => return None,
            },
            _ => return None,
        };
        nums.push(n);
    }
    let result: f64 = match prop {
        "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "abs" if nums.len() == 1 => nums[0].abs(),
        "floor" if nums.len() == 1 => nums[0].floor(),
        "ceil" if nums.len() == 1 => nums[0].ceil(),
        "round" if nums.len() == 1 => nums[0].round(),
        _ => return None,
    };
    Some(Expression::Literal(Box::new(Literal::Number(NumberLiteral {
        value: result,
        raw: Some(format_num(result)),
        span: Span::ZERO,
    }))))
}
